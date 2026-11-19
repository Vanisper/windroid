#!/usr/bin/env python3
"""在 macOS 启动真实 .app，截取首次打开提示并检查工作区及外部资源是否零写入。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import time


def snapshot(root):
    result = {}
    for path in [root, *sorted(root.rglob("*"))]:
        metadata = path.lstat()
        digest = ""
        if stat.S_ISREG(metadata.st_mode):
            try:
                digest = hashlib.sha256(path.read_bytes()).hexdigest()
            except PermissionError:
                digest = "unreadable"
        result[str(path.relative_to(root))] = (
            metadata.st_mode, metadata.st_mtime_ns, metadata.st_ino, digest
        )
    return result


def fixture(root, case, jar):
    home = root / case
    other = root / "external"
    home.mkdir()
    other.mkdir()
    (other / "external-content").write_bytes(b"external resource")
    marker = home / "workspace-format"
    if case in {"legacy", "mixed"}:
        record = home / "instances/legacy/instance.json"
        record.parent.mkdir(parents=True)
        record.write_text(json.dumps({"id": "legacy", "name": "existing", "port": 5554,
            "avd_name": "wd-" + "0" * 32, "spec": {"template": "compact-720",
            "diskBytes": 8 << 30, "image": {"api": 34, "tag": "google_apis_playstore",
            "abi": "arm64-v8a"}}}))
        disk = record.with_name("userdata-qemu.img")
        disk.write_bytes(b"private guest disk")
        disk.chmod(0o640)
        if case == "legacy":
            settings = json.dumps({"runtimeRoot": str(other), "imageRoot": None})
            (home / "storage-locations.json").write_text(settings)
            (home / ".storage-migration.json").write_text(
                json.dumps({"before": json.loads(settings), "packages": []}))
        else:
            marker.write_bytes(b"windroid-workspace-v2\n")
    elif case == "seed":
        dest = home / "runtime/scrcpy-server/5.0.1/scrcpy-server.jar"
        dest.parent.mkdir(parents=True)
        shutil.copyfile(jar, dest)
    elif case == "unreadable":
        marker.write_bytes(b"windroid-workspace-v2\n")
        marker.chmod(0)
    else:
        marker.write_bytes(b"windroid-workspace-v2\n")
        (home / ".storage-migration-v2.json").write_text("{")
    return home


def capture(binary, home, target):
    proc = subprocess.Popen([binary], env={**os.environ, "WINDROID_HOME": str(home)},
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline and proc.poll() is None:
            data = subprocess.check_output(["cua-driver", "call", "list_windows",
                                            json.dumps({"pid": proc.pid})])
            windows = [w for w in json.loads(data)["windows"] if w["is_on_screen"]
                       and w["layer"] == 0 and w["bounds"]["width"] > 100]
            if len(windows) == 1:
                window = windows[0]
                if window["title"] == "Windroid":
                    raise RuntimeError(f"{home}: 显示主窗口，预期为格式提示")
                time.sleep(0.5)
                subprocess.run(["cua-driver", "call", "get_window_state",
                                json.dumps({"pid": proc.pid, "window_id": window["window_id"],
                                            "include_accessibility_tree": False}),
                                "--screenshot-out-file", str(target)], stdout=subprocess.DEVNULL,
                               check=True)
                if target.is_file() and target.stat().st_size:
                    return
            time.sleep(0.15)
        raise RuntimeError(f"{home}: 等待格式提示超时")
    finally:
        if proc.poll() is None:
            proc.terminate()
        proc.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("official_jar", type=Path)
    parser.add_argument("screenshots", type=Path)
    args = parser.parse_args()
    if os.getuid() == 0:
        parser.error("请以普通 macOS 用户执行，以便验证无读取权限的目录")
    args.screenshots.mkdir(parents=True, exist_ok=True)
    for case in ["legacy", "mixed", "transaction", "unreadable", "seed"]:
        with tempfile.TemporaryDirectory(prefix="wd-gate-", dir="/private/tmp") as directory:
            root = Path(directory)
            home = fixture(root, case, args.official_jar)
            before = snapshot(root)
            output = args.screenshots / f"{case}.png"
            capture(args.bundle.resolve() / "Contents/MacOS/windroid", home, output)
            if snapshot(root) != before:
                raise RuntimeError(f"{case}: 工作区或外部资源发生变化")
            print(f"{case}: 零写入，{len(before)} 项，截图 {output}")


if __name__ == "__main__":
    main()

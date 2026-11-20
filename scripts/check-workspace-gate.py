#!/usr/bin/env python3
"""在 macOS 启动真实 .app，操作工作区确认和原生目录选择面板，并核对文件树。"""
import argparse
from contextlib import contextmanager
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
        # macOS 的目录授权扩展属性可能先于 ctime 更新。直接读取其内容。
        attrs = subprocess.run(["/usr/bin/xattr", "-slx", path], capture_output=True, check=False)
        result[str(path.relative_to(root))] = (metadata.st_mode, metadata.st_ino, metadata.st_size,
            metadata.st_mtime_ns, metadata.st_ctime_ns, digest, attrs.returncode, attrs.stdout)
    return result


def fixture(root, case, jar):
    home = root / case
    other = root / "external"
    home.mkdir()
    other.mkdir(exist_ok=True)
    if not (other / "external-content").exists():
        (other / "external-content").write_bytes(b"external resource")
    marker = home / "workspace-format"
    if case == "unmarked":
        (home / "notes.txt").write_text("用户保留的内容")
    elif case == "missing-revision":
        marker.write_bytes(b"windroid-workspace-v1\n")
        folder = home / "instances/device"
        folder.mkdir(parents=True)
        image = {"api": 34, "tag": "google_apis_playstore", "abi": "arm64-v8a"}
        spec = {"template": "compact-720", "diskBytes": 8 << 30, "image": image}
        record = {"id": "device", "name": "Device", "port": 5554, "avd_name": "wd-" + "0" * 32, "spec": spec}
        (folder / "instance.json").write_text(json.dumps(record))
        (folder / "userdata-qemu.img").write_bytes(b"private guest disk")
    elif case in {"seed", "invalid-seed"}:
        dest = home / "runtime/scrcpy-server/5.0.1/scrcpy-server.jar"
        dest.parent.mkdir(parents=True)
        shutil.copyfile(jar, dest)
        if case == "invalid-seed":
            dest.write_bytes(b"incomplete")
    elif case in {"registry-lock", "readonly-lock", "readonly-registry"}:
        marker.write_bytes(b"windroid-workspace-v1\n")
        (home / "instances").mkdir()
        lock = home / "instances/.registry.lock"
        if case == "readonly-registry":
            lock.parent.chmod(0o500)
        else:
            lock.touch()
            lock.chmod(0o400 if case == "readonly-lock" else 0)
    elif case == "unreadable":
        marker.write_bytes(b"windroid-workspace-v1\n")
        marker.chmod(0)
    elif case in {"transaction", "settings"}:
        marker.write_bytes(b"windroid-workspace-v1\n")
        record = "storage.json" if case == "settings" else ".storage-migration.json"
        (home / record).write_text("{")
    return home


class Client:
    def __init__(self, process):
        self.process = process
        self.session = f"workspace-choice-{process.pid}"

    def call(self, tool, args, screenshot=None):
        request = {**args, "pid": self.process.pid}
        if tool != "list_windows":
            request["session"] = self.session
        command = ["cua-driver", "call", tool, json.dumps(request)]
        if screenshot:
            command.extend(["--screenshot-out-file", str(screenshot)])
        result = subprocess.run(command, capture_output=True, text=True, timeout=12)
        if result.returncode and tool != "run_steps":
            raise RuntimeError(f"{tool} 失败：{result.stderr or result.stdout}")
        return json.loads(result.stdout)

    def wait(self, label, role, *, query=None, window_id=None):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and self.process.poll() is None:
            windows = self.call("list_windows", {})["windows"]
            for window in windows:
                if window["bounds"]["width"] < 100:
                    continue
                # AppKit 的警告框可能另有一扇用于合成的可见窗口；按真正的 AX 控件选择。
                wid = window["window_id"]
                if window_id is not None and wid != window_id:
                    continue
                tree = self.call("get_window_state", {"window_id": wid, "query": query or label,
                    "tree_format": "elements", "include_screenshot": False, "max_elements": 400})
                for element in tree.get("elements", []):
                    if element["role"] in role and element.get("enabled") is not False and (
                        label is None or label in (element.get("label"), element.get("value"))
                    ):
                        return wid, element["element_token"]
            time.sleep(0.15)
        raise RuntimeError(f"未在测试客户端找到 {label!r}，进程状态 {self.process.poll()}")

    def click(self, label, role=("AXButton",), *, query=None, window_id=None):
        wid, token = self.wait(label, role, query=query, window_id=window_id)
        self.press(wid, token)

    def press(self, wid, token):
        reply = self.call("run_steps", {"window_id": wid,
            "steps": [{"tool": "click", "args": {"element_token": token}}],
            "observe": {"include_screenshot": False, "max_elements": 8}})
        # 模态框关闭可能早于它对 AXPress 的应答；下一个窗口或进程退出才是成功依据。
        if not reply["ok"] and "-25204" not in str(reply):
            raise RuntimeError(f"原生控件操作失败：{reply}")

    def alert(self, title, home, fragments, screenshot=None):
        wid, _ = self.wait(title, ("AXStaticText",))
        tree = self.call("get_window_state", {"window_id": wid, "tree_format": "elements",
            "include_screenshot": False, "max_elements": 24})
        detail = "\n".join(e.get("value", "") for e in tree.get("elements", [])
                           if e["role"] == "AXStaticText")
        for fragment in (str(home), *fragments):
            if fragment not in detail:
                raise RuntimeError(f"{title} 缺少 {fragment!r}：{detail}")
        if screenshot:
            self.capture(wid, screenshot)
        return wid

    def capture(self, wid, output):
        # AppKit 可在独立的同中心窗口内合成 NSAlert，AX 所在窗口本身是透明的。
        time.sleep(0.4)
        windows = self.call("list_windows", {})["windows"]
        owner = next(w for w in windows if w["window_id"] == wid)
        if not owner["is_on_screen"]:
            def center(window):
                box = window["bounds"]
                return (box["x"] + box["width"] / 2, box["y"] + box["height"] / 2)
            owner = next(w for w in windows if w["is_on_screen"] and center(w) == center(owner))
        self.call("get_window_state", {"window_id": owner["window_id"], "include_accessibility_tree": False}, output)

    def picker(self):
        self.click("选择其他目录…")
        self.wait("选择工作区目录", ("AXWindow",))

    def choose(self, directory):
        self.click(directory.name, ("AXTextField", "AXStaticText", "AXCell", "AXButton"))
        self.click("选择")

    def dismiss_error(self, home, reason, screenshot=None):
        wid, _ = self.wait("alert", ("AXSheet",))
        tree = self.call("get_window_state", {"window_id": wid, "tree_format": "elements",
            "include_screenshot": False, "max_elements": 450})
        sheet = next(e["element_index"] for e in tree["elements"] if e["role"] == "AXSheet")
        children = [e for e in tree["elements"] if e.get("parent_index") == sheet]
        detail = "\n".join(e.get("value", "") for e in children if e["role"] == "AXStaticText")
        if str(home) not in detail or reason not in detail:
            raise RuntimeError(f"选择错误目录后的系统提示缺少路径或原因：{detail}")
        if screenshot:
            self.capture(wid, screenshot)
        dismiss, = [e for e in children if e["role"] == "AXButton"]
        self.press(wid, dismiss["element_token"])

    def opened(self, home):
        self.wait("Windroid", ("AXWindow",))
        if (home / "workspace-format").read_bytes() != b"windroid-workspace-v1\n":
            raise RuntimeError(f"工作区格式标记有误：{home}")
        if not (home / "instances/.registry.lock").is_file():
            raise RuntimeError(f"所选目录未挂载实例库：{home}")

    def exited(self):
        code = self.process.wait(timeout=8)
        if code:
            raise RuntimeError(f"测试客户端异常退出：{code}")


@contextmanager
def launch(binary, home, *, default=False):
    env = {**os.environ, "WINDROID_HOME": str(home), "WINDROID_FAKE": "0"}
    if default:
        if home.name != ".windroid":
            raise ValueError("默认工作区夹具必须名为 .windroid")
        env["HOME"] = str(home.parent)
        del env["WINDROID_HOME"]
    env.pop("WINDROID_UI_DIR", None)
    process = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL)
    client = Client(process)
    try:
        yield client
    finally:
        if process.poll() is None:
            process.terminate()
        process.wait(timeout=10)
        subprocess.run(["cua-driver", "call", "end_session", json.dumps({"session": client.session})],
                       stdout=subprocess.DEVNULL, check=True)


def unchanged(before, path):
    after = snapshot(path)
    differences = [key for key in before.keys() | after.keys() if before.get(key) != after.get(key)]
    if differences:
        raise RuntimeError(f"{path} 发生写入：{sorted(differences)}")


def check_rejections(binary, jar, output):
    cases = {"unmarked": ["格式标记"], "missing-revision": ["revision"],
             "settings": ["storage.json"], "transaction": [".storage-migration.json"],
             "invalid-seed": ["scrcpy"],
             "unreadable": ["Permission denied"],
             "registry-lock": [".registry.lock"], "readonly-lock": [".registry.lock"],
             "readonly-registry": ["instances"]}
    for case, reason in cases.items():
        with tempfile.TemporaryDirectory(prefix="wd-gate-", dir="/private/tmp") as directory:
            root = Path(directory)
            home = fixture(root, case, jar)
            before = snapshot(root)
            with launch(binary, home) as app:
                app.alert("无法打开此工作区", home, reason, output / f"{case}.png")
                app.wait("选择其他目录…", ("AXButton",))
                app.click("退出")
                app.exited()
            unchanged(before, root)
            print(f"{case}: 路径和原因可见，退出后 {len(before)} 项原样保留", flush=True)


def check_reselection(binary, jar, output):
    with tempfile.TemporaryDirectory(prefix="wd-choose-", dir="/private/tmp") as directory:
        root = Path(directory)
        original = fixture(root, "unmarked", jar).rename(root / ".windroid")
        invalid = fixture(root, "missing-revision", jar)
        empty = root / "new-workspace"
        empty.mkdir()
        external = root / "external"
        initial = {p: snapshot(p) for p in (original, invalid, external)}
        with launch(binary, original, default=True) as app:
            app.alert("无法打开此工作区", original, ["格式标记"])
            app.picker()
            # 按 AppKit 的控件标识查找，适用于不同的系统语言。
            app.click(None, query="CancelButton")
            app.alert("无法打开此工作区", original, ["格式标记"])
            app.picker()
            app.choose(invalid)
            app.dismiss_error(invalid, "revision", output / "reselected-invalid.png")
            app.choose(empty)
            app.opened(empty)
        for path, before in initial.items():
            unchanged(before, path)
        # 另起进程重选刚刚建好的工作区，核对其标记和仅此处存在的注册表锁。
        marker = (empty / "workspace-format").stat()
        (empty / "instances/.registry.lock").unlink()
        with launch(binary, original) as app:
            app.alert("无法打开此工作区", original, ["格式标记"])
            app.picker()
            app.choose(empty)
            app.opened(empty)
        reopened = (empty / "workspace-format").stat()
        if (marker.st_ino, marker.st_mtime_ns) != (reopened.st_ino, reopened.st_mtime_ns):
            raise RuntimeError("重开工作区时重写了原格式标记")
        for path, before in initial.items():
            unchanged(before, path)
        print("目录面板：取消、重选无效目录、在新目录建库、重开已有库均通过", flush=True)


def check_seed(binary, jar, output):
    with tempfile.TemporaryDirectory(prefix="wd-seed-", dir="/private/tmp") as directory:
        root = Path(directory)
        home = fixture(root, "seed", jar)
        before = snapshot(root)
        with launch(binary, home) as app:
            app.alert("使用此目录新建工作区？", home, ["scrcpy-server"], output / "seed.png")
            unchanged(before, root)
            app.click("取消")
            app.exited()
        unchanged(before, root)
        original = fixture(root, "unmarked", jar)
        resources = snapshot(home / "runtime")
        protected = snapshot(original)
        seed = snapshot(home)
        with launch(binary, original) as app:
            app.picker()
            app.choose(home)
            alert = app.alert("使用此目录新建工作区？", home, ["scrcpy-server"])
            unchanged(seed, home)
            app.click("取消", window_id=alert)
            app.wait("选择工作区目录", ("AXWindow",))
            unchanged(seed, home)
            app.choose(home)
            app.alert("使用此目录新建工作区？", home, ["scrcpy-server"])
            app.click("在此新建工作区")
            app.opened(home)
        unchanged(protected, original)
        unchanged(resources, home / "runtime")
        print("预放官方 JAR：确认前及取消零写入，确认后建库，共享资源保留", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("official_jar", type=Path)
    parser.add_argument("screenshots", type=Path)
    args = parser.parse_args()
    if os.getuid() == 0:
        parser.error("请以普通 macOS 用户执行，以便验证无读取权限的目录")
    binary = args.bundle.resolve() / "Contents/MacOS/windroid"
    args.screenshots.mkdir(parents=True, exist_ok=True)
    check_rejections(binary, args.official_jar, args.screenshots)
    check_reselection(binary, args.official_jar, args.screenshots)
    check_seed(binary, args.official_jar, args.screenshots)


if __name__ == "__main__":
    main()

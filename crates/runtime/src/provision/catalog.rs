//! 发布时固定的目录。网络只传输以下档案，不从远端清单接收路径、命令或摘要。
//! Google 固定包的 SHA-1、字节数、修订号及 uses-license 来自官方仓库：
//! <https://dl.google.com/android/repository/repository2-3.xml> 和
//! <https://dl.google.com/android/repository/sys-img/google_apis_playstore/sys-img2-3.xml>。
//! 解压字节数与顶层名来自这些包的 ZIP 中央目录。

pub(super) const SDK_LICENSE: &str = include_str!("licenses/android-sdk-license.txt");
const IMAGE_LICENSE: &str = include_str!("licenses/android-sdk-arm-dbt-license.txt");
const APACHE_LICENSE: &str = include_str!("licenses/scrcpy-apache-2.0.txt");
pub(super) const IMAGE_DIR: &str = "system-images/android-34/google_apis_playstore/arm64-v8a";
pub(super) const BUILD_TOOLS: &str = "build-tools/34.0.0";
pub(super) const EMULATOR_VERSION: &str = "37.2.13";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Emulator,
    Adb,
    Aapt,
    Image,
    Server,
}

#[derive(Clone, Copy)]
pub(super) enum Digest {
    Sha1(&'static str),
    Sha256(&'static str),
}

impl Digest {
    pub fn hex(self) -> &'static str {
        match self {
            Self::Sha1(h) | Self::Sha256(h) => h,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Artifact {
    pub kind: Kind,
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    pub digest: Digest,
    /// None 表示直接安装单文件。ZIP 统一去掉一个固定的顶层目录。
    pub prefix: Option<&'static str>,
    pub unpacked: u64,
    /// 启动必需的文件及其 ZIP 内原始长度。重扫只需 stat 就能识别截断。
    pub essentials: &'static [(&'static str, u64)],
}

impl Artifact {
    pub fn license(self) -> (&'static str, &'static str, &'static str) {
        match self.kind {
            Kind::Image => (
                "Android SDK License + ARM DBT",
                "https://dl.google.com/android/repository/sys-img/google_apis_playstore/sys-img2-3.xml#android-sdk-arm-dbt-license",
                IMAGE_LICENSE,
            ),
            Kind::Server => (
                "Apache License 2.0",
                "https://github.com/Genymobile/scrcpy/blob/v5.0.1/LICENSE",
                APACHE_LICENSE,
            ),
            _ => (
                "Android SDK License",
                "https://developer.android.com/studio/terms",
                SDK_LICENSE,
            ),
        }
    }
}

pub(super) const CATALOG: [Artifact; 5] = [
    Artifact {
        kind: Kind::Emulator,
        id: "emulator",
        name: "Android 模拟器",
        version: EMULATOR_VERSION,
        url: "https://dl.google.com/android/repository/emulator-darwin_aarch64-16740729.zip",
        bytes: 416_113_219,
        digest: Digest::Sha1("6c5e61b873e11c7d1d08683bea089f762baee070"),
        prefix: Some("emulator"),
        unpacked: 1_309_741_043,
        essentials: &[
            ("emulator", 9_178_208),
            ("qemu/darwin-aarch64/qemu-system-aarch64", 60_384_896),
            (
                "qemu/darwin-aarch64/qemu-system-aarch64-headless",
                50_388_128,
            ),
        ],
    },
    Artifact {
        kind: Kind::Adb,
        id: "platform-tools",
        name: "设备连接工具",
        version: "37.0.1",
        url: "https://dl.google.com/android/repository/platform-tools_r37.0.1-darwin.zip",
        bytes: 16_110_554,
        digest: Digest::Sha1("6ae73f4de6452dc57e62ec02b68eed92a4c21661"),
        prefix: Some("platform-tools"),
        unpacked: 38_814_256,
        essentials: &[("adb", 18_844_128)],
    },
    Artifact {
        kind: Kind::Aapt,
        id: "build-tools",
        name: "应用包解析工具",
        version: "34.0.0",
        url: "https://dl.google.com/android/repository/build-tools_r34-macosx.zip",
        bytes: 76_559_180,
        digest: Digest::Sha1("00e1301387028f33753e38274401694e1a27c62f"),
        prefix: Some("android-14"),
        unpacked: 196_201_746,
        essentials: &[("aapt", 2_607_488)],
    },
    Artifact {
        // 发布者提供的 SHA-256：<https://github.com/Genymobile/scrcpy/releases/download/v5.0.1/SHA256SUMS.txt>
        kind: Kind::Server,
        id: "scrcpy-server",
        name: "画面与输入服务",
        version: "5.0.1",
        url: "https://github.com/Genymobile/scrcpy/releases/download/v5.0.1/scrcpy-server-v5.0.1",
        bytes: 733_930,
        digest: Digest::Sha256("764eb6f79811d5211fe9df341120882ba9994c7a61b897d7bf3fb662e53bc536"),
        prefix: None,
        unpacked: 733_930,
        essentials: &[],
    },
    Artifact {
        kind: Kind::Image,
        id: "system-image",
        name: "Android 14 系统镜像",
        version: "14",
        url: "https://dl.google.com/android/repository/sys-img/google_apis_playstore/arm64-v8a-34_r14.zip",
        bytes: 1_548_905_381,
        digest: Digest::Sha1("c307c3301dc52635ebc78b943c39b3c377856ebc"),
        prefix: Some("arm64-v8a"),
        unpacked: 3_341_086_447,
        essentials: &[
            ("system.img", 3_191_865_344),
            ("vendor.img", 95_420_416),
            ("kernel-ranchu", 12_784_248),
            ("ramdisk.img", 1_539_730),
            ("userdata.img", 1_048_576),
            ("encryptionkey.img", 18_874_368),
        ],
    },
];

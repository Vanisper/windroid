#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "Usage: $0 [OUTPUT_DIR]"
    echo 'OUTPUT_DIR defaults to build/test-apks under the repository.'
    echo 'Build two signed test APKs for Android 14 using the local JDK and SDK.'
    echo "Uses ANDROID_HOME (or ANDROID_SDK_ROOT), platforms/android-34, and build-tools/$build_tools_version."
    echo 'Set ANDROID_BUILD_TOOLS_VERSION to select a different installed version.'
    echo 'The private test signing key stays in OUTPUT_DIR for repeat installations.'
}
die() { echo "build-test-apks: $*" >&2; exit 1; }

build_tools_version=${ANDROID_BUILD_TOOLS_VERSION:-34.0.0}
if [[ ${1:-} == --help || ${1:-} == -h ]]; then usage; exit 0; fi
script_dir=$(cd -- "$(dirname -- "$0")" && pwd -P)
if [[ $# == 0 ]]; then set -- "$script_dir/../build/test-apks"; fi
[[ $# == 1 && -n $1 ]] || { usage >&2; exit 2; }

umask 077
min_sdk=24
target_sdk=34
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-"$HOME/Library/Android/sdk"}}
android_jar="$sdk/platforms/android-$target_sdk/android.jar"
bt="$sdk/build-tools/$build_tools_version"
[[ -r $android_jar ]] || die "missing Android 14 platform: $android_jar"
for tool in aapt2 d8 zipalign apksigner; do
    [[ -x $bt/$tool ]] || die "missing build tool: $bt/$tool"
done
if [[ -n ${JAVA_HOME:-} ]]; then export PATH="$JAVA_HOME/bin:$PATH"; fi
for tool in java javac keytool zip; do
    command -v "$tool" >/dev/null || die "missing from PATH: $tool"
done

mkdir -p -- "$1"
out=$(cd -- "$1" && pwd -P)
work=$(mktemp -d "$out/.windroid-apks.XXXXXXXX")
trap 'rm -rf -- "${work:?}"' EXIT

# Both manifests use the same fully qualified activity, so the bytecode is built once.
mkdir -p "$work/classes" "$work/dex" "$work/res/values"
javac --release 8 -Xlint:-options -encoding UTF-8 -classpath "$android_jar" \
    -d "$work/classes" "$script_dir/android-fixture/MainActivity.java"
"$bt/d8" --release --min-api "$min_sdk" --lib "$android_jar" --output "$work/dex" \
    "$work/classes/dev/windroid/fixture/"*.class

# This password protects a local, disposable test identity, scoped to OUTPUT_DIR.
export WINDROID_FIXTURE_STORE_PASS=windroid-test
key="$out/.windroid-test.p12"
if [[ ! -f $key ]]; then
    keytool -genkeypair -storetype PKCS12 -keystore "$work/test.p12" -alias fixture \
        -keyalg RSA -keysize 2048 -validity 3650 -dname 'CN=Windroid Test Fixture' \
        -storepass:env WINDROID_FIXTURE_STORE_PASS -keypass:env WINDROID_FIXTURE_STORE_PASS -noprompt
    # A second build in the same output directory must use the same signing identity.
    ln "$work/test.p12" "$key" || [[ -f $key ]]
fi

build_apk() {
    local package="dev.windroid.test.$1" label="Windroid $2"
    # The app catalog reads application-label from the compiled resource table.
    printf '<resources><string name="app_name">%s</string></resources>\n' "$label" > "$work/res/values/strings.xml"
    cat > "$work/AndroidManifest.xml" <<MANIFEST
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="$package"
    android:versionCode="1" android:versionName="1.0">
    <uses-sdk android:minSdkVersion="$min_sdk" android:targetSdkVersion="$target_sdk" />
    <application android:label="@string/app_name" android:allowBackup="false" android:theme="@android:style/Theme.Material.Light">
        <activity android:name="dev.windroid.fixture.MainActivity" android:exported="true"
            android:windowSoftInputMode="stateAlwaysHidden|adjustResize">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
MANIFEST
    "$bt/aapt2" compile --dir "$work/res" -o "$work/resources.zip"
    "$bt/aapt2" link -I "$android_jar" --manifest "$work/AndroidManifest.xml" \
        -o "$work/base.apk" "$work/resources.zip"
    zip -q -j -X "$work/base.apk" "$work/dex/classes.dex"
    "$bt/zipalign" -f 4 "$work/base.apk" "$work/aligned.apk"
    "$bt/apksigner" sign --ks "$key" --ks-key-alias fixture --ks-pass env:WINDROID_FIXTURE_STORE_PASS \
        --v4-signing-enabled false --out "$work/$package.apk" "$work/aligned.apk"
    "$bt/zipalign" -c 4 "$work/$package.apk"
    "$bt/apksigner" verify "$work/$package.apk"
    local permissions
    permissions=$("$bt/aapt2" dump permissions "$work/$package.apk")
    [[ $permissions != *uses-permission* ]] || die "unexpected permissions in $package"
}

build_apk alpha Alpha
build_apk beta Beta
for apk in "$work"/dev.windroid.test.*.apk; do
    mv -f -- "$apk" "$out/"
    printf '%s\n' "$out/${apk##*/}"
done

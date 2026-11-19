#!/usr/bin/env bash
# Android build preflight for the EffiBuddy phone shell.
#
# This script never installs anything. It reports what is missing and, once the
# toolchain is complete, runs the Rust half of the build. The Gradle/Android
# half lives in `app/android` and is documented in docs/mobile/MOBILE_BUILD.md.
#
# Usage:
#   bash scripts/build-android.sh            # preflight only (exit 0)
#   bash scripts/build-android.sh --build    # preflight, then cargo ndk build
set -euo pipefail

cd "$(dirname "$0")/.."

DO_BUILD=0
if [[ "${1:-}" == "--build" ]]; then
  DO_BUILD=1
fi

missing=0
note() { printf '  %-28s %s\n' "$1" "$2"; }
check() { # name, command, hint
  if command -v "$2" >/dev/null 2>&1; then
    note "$1" "ok ($2)"
  else
    note "$1" "缺失 → $3"
    missing=1
  fi
}

echo "== EffiBuddy Android preflight =="
check "Rust"                cargo       "https://rustup.rs"
check "cargo-ndk"           cargo-ndk   "cargo install cargo-ndk"
check "JDK 17"              java        "安装 JDK 17 并设置 JAVA_HOME"
check "Android SDK"         sdkmanager  "Android Studio 或 commandline-tools"
check "Android NDK"         ndk-build   "sdkmanager 'ndk;27.0.12077973'"
check "adb"                 adb         "platform-tools"

for target in aarch64-linux-android x86_64-linux-android; do
  if rustup target list --installed 2>/dev/null | grep -qx "$target"; then
    note "rust target $target" "ok"
  else
    note "rust target $target" "缺失 → rustup target add $target"
    missing=1
  fi
done

if [[ -n "${ANDROID_NDK_HOME:-}" || -n "${NDK_HOME:-}" ]]; then
  note "ANDROID_NDK_HOME" "ok"
else
  note "ANDROID_NDK_HOME" "缺失 → 指向 NDK 目录"
  missing=1
fi

echo
if [[ $missing -ne 0 ]]; then
  echo "工具链不完整；先按上面的提示补全，再运行：bash scripts/build-android.sh --build"
  echo "详细方案见 docs/mobile/MOBILE_BUILD.md（B 轨选型与系统能力矩阵）。"
  exit 0
fi

echo "工具链齐全，可执行的构建目标："
echo "  cargo ndk -t arm64-v8a -t x86_64 -o app/android/jniLibs build --release -p effibuddy-mobile -p effibuddy-agent-core"

if [[ $DO_BUILD -eq 1 ]]; then
  mkdir -p app/android/jniLibs
  cargo ndk -t arm64-v8a -t x86_64 -o app/android/jniLibs build --release \
    -p effibuddy-mobile -p effibuddy-agent-core
  echo "完成：app/android/jniLibs"
  echo "接着：(cd app/android && ./gradlew assembleDebug)"
else
  echo "加上 --build 可立即编译 Rust 侧产物。"
fi

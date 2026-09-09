#!/usr/bin/env bash
set -euo pipefail
case "$VALIDATION_CASE" in
  host)
    python3 .github/boringcache-time.py build cargo build --locked -p bingle_test --features localnet --bin localnet_e2e_provisioner
    test -x target/debug/localnet_e2e_provisioner
    file target/debug/localnet_e2e_provisioner > "$RUNNER_TEMP/validation/output.txt"
    ;;
  android)
    export BINGLE_ANDROID_ABIS=x86_64
    python3 .github/boringcache-time.py build bash bingle_jsi/scripts/build_android.sh
    file bingle_jsi/android/src/main/jniLibs/x86_64/libbingle_jsi.so > "$RUNNER_TEMP/validation/output.txt"
    git diff --exit-code -- Cargo.lock
    ;;
  *) exit 2 ;;
esac

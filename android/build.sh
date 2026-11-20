#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Builds joy2con.apk without Gradle: aapt2 -> javac -> d8 -> zipalign -> apksigner.
# Needs ANDROID_HOME (default ~/Android/Sdk) with platforms;android-36 and
# build-tools, and a JDK 17 (JAVA_HOME, default ~/Android/jdk17).
set -euo pipefail
cd "$(dirname "$0")"

SDK=${ANDROID_HOME:-$HOME/Android/Sdk}
export JAVA_HOME=${JAVA_HOME:-$HOME/Android/jdk17}
export PATH=$JAVA_HOME/bin:$PATH
BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
JAR=$SDK/platforms/android-36/android.jar
KS=${JOY2CON_KEYSTORE:-$HOME/.android/joy2con-debug.keystore}
VERSION_CODE=${VERSION_CODE:-1}
VERSION_NAME=${VERSION_NAME:-0.1.0}

rm -rf build && mkdir -p build/gen build/classes build/dex
"$BT/aapt2" compile --dir res -o build/res.zip
"$BT/aapt2" link -o build/base.apk -I "$JAR" --manifest AndroidManifest.xml build/res.zip \
    --java build/gen --min-sdk-version 33 --target-sdk-version 36 \
    --version-code "$VERSION_CODE" --version-name "$VERSION_NAME"
javac -source 17 -target 17 -encoding UTF-8 -Xlint:-options -classpath "$JAR" -d build/classes \
    $(find src build/gen -name '*.java')
"$BT/d8" --min-api 33 --lib "$JAR" --output build/dex $(find build/classes -name '*.class')
(cd build/dex && zip -q ../base.apk classes.dex)
"$BT/zipalign" -p -f 4 build/base.apk build/aligned.apk
if [[ ! -f $KS ]]; then
    mkdir -p "$(dirname "$KS")"
    keytool -genkeypair -keystore "$KS" -storepass android -keypass android -alias joy2con \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=joy2con debug" >/dev/null
fi
"$BT/apksigner" sign --ks "$KS" --ks-pass pass:android --key-pass pass:android --ks-key-alias joy2con \
    --out build/joy2con.apk build/aligned.apk
"$BT/apksigner" verify build/joy2con.apk
echo "built $(pwd)/build/joy2con.apk"

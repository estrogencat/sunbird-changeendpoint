#!/usr/bin/env bash
# Downloads Discord $VERSION's splits from $TRACKER, checks them against the
# checked-out release and writes discord.json, which phones download and
# check each split against. Run by .github/workflows/discord.yml inside
# `nix develop`, from the release's checkout with `cargo build --release -p
# sigscan`, and with $PATCHER (default target/release/patcher) the patcher
# that has `--check-host`.
set -euo pipefail

: "${VERSION:?}" "${RELEASE:?}" "${TRACKER:?}" "${DISCORD_CERT:?}"

tools=$(ls -d "$ANDROID_SDK_ROOT"/build-tools/* | sort -V | tail -1)
splits=(base)
for abi in arm64_v8a armeabi_v7a x86_64; do splits+=("config.$abi"); done
for dpi in ldpi mdpi tvdpi hdpi xhdpi xxhdpi xxxhdpi; do splits+=("config.$dpi"); done
# Android names language splits by language alone; the tracker 404s on the
# ones Discord doesn't ship.
for lang in af am ar as az be bg bn bs ca cs da de el en es et eu fa fi fr gl gu he hi hr hu \
   hy id in is it iw ja ka kk km kn ko ky lo lt lv mk ml mn mr ms my nb ne nl no or pa pl pt ro \
   ru si sk sl sq sr sv sw ta te th tl tr uk ur uz vi zh zu; do
   splits+=("config.$lang")
done

mkdir -p splits
fetched=()
for split in "${splits[@]}"; do
   code=$(curl -sSL --retry 3 -o "splits/$split.apk" -w '%{http_code}' "$TRACKER/download/$VERSION/$split")
   case $code in
      200) fetched+=("$split") ;;
      404) rm -f "splits/$split.apk" ;;
      *)
         echo "::error::$split: HTTP $code"
         exit 1
         ;;
   esac
done
for need in base config.arm64_v8a config.armeabi_v7a; do
   [ -f "splits/$need.apk" ] || { echo "::error::$VERSION has no $need split"; exit 1; }
done

for split in "${fetched[@]}"; do
   certs=$("$tools/apksigner" verify --print-certs "splits/$split.apk" |
      sed -n 's/^Signer #[0-9]* certificate SHA-256 digest: //p' | sort -u)
   if [ "$certs" != "$DISCORD_CERT" ]; then
      echo "::error::$split is not signed with Discord's certificate: $certs"
      exit 1
   fi
done

for abi in arm64_v8a armeabi_v7a; do
   unzip -p "splits/config.$abi.apk" "lib/${abi/_/-}/liblibdiscord_wasm_host.so" > "$abi.so"
   target/release/sigscan "$abi.so" check
done
python3 scripts/extract-schema.py splits/base.apk --check
"${PATCHER:-target/release/patcher}" --check-host splits/base.apk

name=$("$tools/aapt2" dump badging splits/base.apk | sed -n "s/.*versionName='\([^']*\)'.*/\1/p" | head -1)
for split in "${fetched[@]}"; do
   file="splits/$split.apk"
   printf '%s\t%s\t%s\n' "$split" "$(sha256sum "$file" | cut -d' ' -f1)" "$(stat -c %s "$file")"
done | jq -Rn \
   --argjson code "$VERSION" \
   --arg name "$name" \
   --arg release "$RELEASE" \
   --arg url "$TRACKER/download/$VERSION/" \
   '{
      versionCode: $code,
      versionName: $name,
      verifiedWith: $release,
      url: $url,
      splits: ([inputs | split("\t") | {(.[0]): {sha256: .[1], size: (.[2] | tonumber)}}] | add)
   }' > discord.json
echo "Discord $VERSION ($name) is ready: ${#fetched[@]} splits"

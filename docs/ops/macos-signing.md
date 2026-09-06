# macOS Signing

Use a Developer ID Application certificate, not an Apple Development or ad-hoc
identity. Keep credentials in Keychain, never in this repository. The account
holder must accept any required Apple agreements before notarization works.

## Preflight

```sh
security find-identity -v -p codesigning
export APPLE_SIGNING_IDENTITY='Developer ID Application: YOUR NAME (TEAMID)'
export APPLE_NOTARY_PROFILE='YOUR_KEYCHAIN_PROFILE'
xcrun notarytool history --keychain-profile "$APPLE_NOTARY_PROFILE"
```

Stop on an authentication or agreement error. A valid signing certificate alone
does not prove notarization readiness.

## Sign The Reviewed Artifact

Use a staged archive directory containing the verified binary for the exact
release commit. Do not sign or replace a running installation. Set `package`
to the absolute staged directory and `name` to its directory name.

```sh
codesign --force --sign "$APPLE_SIGNING_IDENTITY" --options runtime \
  --timestamp "$package/lowdown"
codesign --verify --strict --verbose=2 "$package/lowdown"
codesign --display --verbose=4 "$package/lowdown"
"$package/lowdown" --version
ditto --norsrc --noextattr --noacl -c -k --keepParent "$package" "$name-notary.zip"
```

Signing changes the bytes: repeat binary smoke checks and regenerate the final
archive and checksum after signing. Keep unsigned CI and signed distribution
checksums distinct.
Inspect the ZIP entries: only the executable and `scripts/release-files.txt`
payload may be present. Do not include AppleDouble (`._*`) metadata files.

## Submit Only After Approval

This sends the staged software to Apple, but does not publish a GitHub release:

```sh
xcrun notarytool submit "$name-notary.zip" \
  --keychain-profile "$APPLE_NOTARY_PROFILE" --wait --timeout 20m
```

Record the submission ID, require `Accepted`, and inspect the log with
`xcrun notarytool log SUBMISSION_ID --keychain-profile "$APPLE_NOTARY_PROFILE"`.
If waiting times out, inspect that same submission instead of uploading again.

Apple issues tickets for standalone executables but cannot staple tickets to
them or ZIP files. Archive-based CLI distribution therefore needs an online
Gatekeeper lookup. Do not claim offline trust, run `stapler` on the bare binary,
or disable Gatekeeper. A stapled DMG/PKG would be a separate distribution choice.

Check the notarization ticket for the standalone executable:

```sh
codesign -vvvv -R=notarized --check-notarization "$package/lowdown"
```

Do not use the app-specific `spctl --type execute` assessment for this bare CLI;
it can reject valid code because it is not an app bundle. Then verify the final
download on a clean Mac with quarantine intact, including first launch with
network access. A local signature or ticket check does not prove that download
experience. See [Apple's product testing guidance](https://developer.apple.com/forums/thread/130560).

Reference: [Apple's notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

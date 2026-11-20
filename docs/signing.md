# Code signing

Release builds are signed so that Windows SmartScreen and macOS Gatekeeper
open Pulse without an "unknown publisher" warning. The release workflow
(`.github/workflows/release.yml`) signs only when the credentials below exist,
and only for `v*` tags (Windows) or whenever the Apple secrets are present
(macOS). Without them it produces the same unsigned builds as v0.1.0, so
nothing breaks while the accounts are being set up.

Linux packages are not signed this way; the `.deb` and tarball are published
with the release, and `install.sh` downloads over HTTPS from GitHub.

## Windows: SignPath Foundation (free for open source)

SignPath Foundation signs open source projects with its own certificate, so
the publisher shown to users is "SignPath Foundation", and the build has to
come from GitHub Actions on GitHub-hosted runners. The signing key never
leaves SignPath.

Their conditions ([signpath.org/terms](https://signpath.org/terms)) that this
repo already meets: OSI license (AGPL-3.0), no proprietary components (the
OpenH264 library is downloaded by the user's computer from Cisco at runtime
and is not part of the signed package), already released, documented. What
the maintainers have to do:

1. Apply at <https://signpath.org/apply> with the repository URL. SignPath
   checks project reputation by hand, so a brand-new repository may be asked
   to wait or may be declined. Everyone who can approve a release needs
   two-factor authentication on SignPath and GitHub.
2. In SignPath, create the project with slug `pulse`, add the **GitHub.com**
   trusted build system and link it to the project, and create:
   - an artifact configuration named `initial`: paste
     [`packaging/signpath/artifact-configuration.xml`](../packaging/signpath/artifact-configuration.xml)
     (it requires the embedded product name `Pulse` and the version passed by
     the workflow; `crates/pulse/build.rs` embeds both in `pulse.exe`)
   - a signing policy named `release-signing` with manual approval
3. Create a submitter user and an API token for it, then in the GitHub
   repository settings add the secret `SIGNPATH_API_TOKEN` and the variable
   `SIGNPATH_ORGANIZATION_ID` (the organization ID is on SignPath's
   organization page).
4. Tag a release. The Windows job uploads `pulse.exe`, SignPath waits for an
   approver, and the signed exe goes into `pulse-windows-x86_64.zip`.

SignPath also requires the README to carry a "Code signing policy" section
(it does) and that each project member follows its roles: Authors, Reviewers
and Approvers. For now the repository owner is all three; name more people in
the README as they join.

If SignPath declines, the workflow step can be swapped for Azure Artifact
Signing or a purchased certificate; the Windows job only needs to leave a
signed `pulse.exe` in `signed/`.

## macOS: Developer ID and notarization

Apple has no free route: this needs the Apple Developer Program (US$99/year).
`scripts/sign-macos-app.sh` then signs `Pulse.app` with the hardened runtime,
submits it to Apple's notary service, staples the ticket and checks it with
`spctl`.

1. Enroll at <https://developer.apple.com/programs/>.
2. Create a **Developer ID Application** certificate (Certificates, IDs &
   Profiles), export it with its private key from Keychain Access as a `.p12`
   with a password.
3. In App Store Connect, Users and Access, Integrations, create a Team API
   key with the Developer role and download the `.p8` (Apple only offers the
   download once). Note the key ID and the issuer ID shown above the keys list.
4. Add these GitHub repository secrets:

   | Secret | Value |
   | --- | --- |
   | `MACOS_CERTIFICATE_P12` | `base64 -i cert.p12` |
   | `MACOS_CERTIFICATE_PASSWORD` | the `.p12` password |
   | `APPLE_API_KEY_P8` | `base64 -i AuthKey_XXXX.p8` |
   | `APPLE_API_KEY_ID` | the key ID |
   | `APPLE_API_ISSUER_ID` | the issuer ID (a UUID) |

5. Tag a release. Notarization usually takes a few minutes.

The only entitlement requested is `com.apple.security.cs.disable-library-validation`
([`packaging/macos/entitlements.plist`](../packaging/macos/entitlements.plist)).
The hardened runtime otherwise refuses to load libraries not signed by the same
team, and Pulse loads Cisco's OpenH264 after checking its SHA-256. Nothing
else is requested.

## Cutting a signed release

Signing changes the binaries, so the hashes in the package manifests
(`packaging/windows`, `packaging/homebrew`, `packaging/aur`) change too: tag a
new version (for example v0.1.1) after the credentials are set up, then update
the manifests from that release. The v0.1.0 assets stay unsigned.

# Releasing chopsuey

Everything that has to be true before a build goes to anyone else, then the steps.

## Blockers: none of these can ship as they are

| What | Where | Why |
|---|---|---|
| Stem model licence | `models/`, `src/stems.rs` | The HTDemucs weights may be research-only. Swap or license the model. |
| Production licence keypair | `PUBLIC_KEY` in `src/licence.rs` | It is the development key, whose private half sits in `tools/licence_keygen/dev-private-key.txt`. Make a new pair with `cargo run -p licence_keygen -- new-keypair --out <somewhere outside the repo>`, paste the printed public key, and keep the private key in a password manager. |
| Vendor URL and email | `URL` and `EMAIL` in `src/lib.rs`; `CLAP_MANUAL_URL` and `CLAP_SUPPORT_URL` | Hosts show them, and users need somewhere to get help. |
| Release repository | `LATEST_RELEASE_API` in `src/update.rs` | The update check reads this repo's latest GitHub release, so the repo must be public, or point it at another one. |
| Developer ID certificates | `DEVELOPER_ID_APP`, `DEVELOPER_ID_INSTALLER`, `NOTARY_PROFILE` (see `scripts/release.sh`) | Without them macOS blocks the installer on other Macs. |

Never change these after the first release. Hosts use them to find the plugin in saved sets:
- VST3 class ID `TempoSamplerV001`
- CLAP ID `com.torontoaudiocompany.chopsuey`
- AU codes `aumu Chsy TAud`

## Steps

1. Bump `version` in `Cargo.toml`. Hosts show it, and the update check compares it with release tags.
2. Move the `Unreleased` section of `CHANGELOG.md` under the new version.
3. Run `scripts/validate.sh`. It must pass all of these:
   - pluginval at strictness 10, on both the release build and the `rt-checks` debug build
   - clap-validator
   - the VST3 validator
   - auval
4. Work through [host-testing.md](host-testing.md) in at least Live 12 and one other host.
5. Run `scripts/release.sh` to build the signed, notarized `.pkg`.
6. Install the `.pkg` on a Mac that has never had chopsuey and open a set with it.
7. Tag `vX.Y.Z`, push, and create a GitHub release with the `.pkg` and the changelog. The tag is what the update check reads.

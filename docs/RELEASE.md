# Releasing chopsuey

Everything that has to be true before a build goes to anyone else, then the steps.

## Blockers: none of these can ship as they are

| What | Where | Why |
|---|---|---|
| Stem model licence | `models/`, `src/stems.rs` | The HTDemucs weights may be research-only. Swap or license the model. |
| Vendor URL and email | `URL` and `EMAIL` in `src/lib.rs`; `CLAP_MANUAL_URL` and `CLAP_SUPPORT_URL` | Hosts show them, and users need somewhere to get help. |
| Release repository | `LATEST_RELEASE_API` in `src/update.rs` | The update check reads the latest GitHub release of `Toronto-Audio-Company/chopsuey` without signing in. That repo is private, so the check finds nothing until it's public (or releases go to a public repo and the URL points there). |
| nih-plug's CLAP state handling | nih-plug `src/wrapper/clap/wrapper.rs`, `ext_state_load` | A corrupt saved CLAP or AU state makes it try to allocate whatever length the file claims, which crashes the host (clap-validator `state-invalid-random`). After loading state it also never asks the host to rescan parameter values (`state-reproducibility-*`). Both fixes are a few lines. Carry them in a fork via `[patch]` until they're upstream. |
| Developer ID certificates | `DEVELOPER_ID_APP`, `DEVELOPER_ID_INSTALLER`, `NOTARY_PROFILE` (see `scripts/release.sh`) | Without them macOS blocks the installer on other Macs. |

Decided, written down so they're not a surprise:
- **Minimum macOS is 13.4.** That's what ONNX Runtime and Dawn are built for. The plugin itself would load on 11, but stem splitting wouldn't.
- **Stem splitting needs Apple silicon.** ONNX Runtime ships no Intel Mac build. On Intel Macs the plugin works and says stems are unavailable.
- **The stem splitter is installed once** to `/Library/Application Support/Toronto Audio Company/chopsuey`: the helper, Dawn and the model. It isn't in every bundle. Check that a sandboxed AU host (Logic, GarageBand) can start it, as part of the host pass.

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

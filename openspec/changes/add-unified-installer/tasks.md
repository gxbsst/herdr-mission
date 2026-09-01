## 1. Installer Contract And Payload

- [x] 1.1 Add public integration RED/GREEN tests for fixed-version Codex, Claude Code and combined noninteractive installs.
- [x] 1.2 Add the canonical `herdr-mission-team` skill payload and implement platform, argument, TTY and release download handling in `install.sh`.
- [x] 1.3 Verify CLI and skill checksums before side effects, then atomically install the CLI plus owned canonical and selected Agent skill copies.

## 2. Failure Closing And Idempotency

- [x] 2.1 Add RED/GREEN tests for checksum mismatch, missing Herdr, unsupported platform and `PATH` warning behavior.
- [x] 2.2 Add RED/GREEN tests for repeat installation, owned upgrade and foreign canonical/Agent skill conflicts with unchanged pre-call targets.
- [x] 2.3 Verify plugin resolved commit, CLI version and skill frontmatter after installation, returning a clear nonzero failure on any mismatch.

## 3. Release And Documentation

- [x] 3.1 Extend the Release workflow to stamp `install.sh`, build the skill archive and include CLI/skill checksums and all installer assets.
- [x] 3.2 Update README with latest and pinned `curl | sh` commands, noninteractive Agent selection, installed paths and the plugin-only fallback.

## 4. Verification

- [x] 4.1 Run installer integration tests plus the complete locked Rust test suite.
- [x] 4.2 Run `cargo fmt --check`, all-target/all-feature Clippy with warnings denied, release build and focused/all OpenSpec strict validation.
- [x] 4.3 Run Git diff and whitespace checks, then review the real diff for unsafe overwrite, mutable-version or unrelated-worktree changes.

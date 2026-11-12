# xAI credential file name goldens

`../../fixtures/xai_filename_go.json` holds the file names Go gives a new xAI login
credential (`CredentialFileName`) and the names it recognizes from before the identity
hash (`legacyCredentialFileName`). The functions come verbatim from CLIProxyAPI
`internal/auth/xai/filename.go` at v8.0.67 (`8bba7a77`), where d65840fb introduced them.
The cases cover case-folded emails, subjects, distinct accounts whose sanitized names
collide, identities that sanitize to nothing and a long email.
`xai_auth::tests::credential_file_names_match_go_v8_0_67` replays them.

The program uses the standard library only. It was recorded with Go 1.27.3; nothing in
these functions depends on the toolchain version.

```sh
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/xai_filename/main.go" "$tmp/"
(
  cd "$tmp"
  go mod init xaifilename
  GOTOOLCHAIN=local go run . "$crate/tests/fixtures/xai_filename_go.json"
)
rm -rf "$tmp"
```

# Naming (owner's rule, 2026-11-05, DECISION S44)

Every new name follows `docs/NAMING.md`: code, commands, flags, scripts, launchd jobs, macOS users,
paths, data files, config fields, Keychain items, provider-side keys, ids and commit subjects.

- **Read `docs/NAMING.md` before naming anything new.** Use its vocabulary, with one word per idea
  and no new abbreviations.
- **If no rule fits,** add one to `docs/NAMING.md` in the same change, with an example.
- **Never rename an installed identity in place:** OS users, Keychain items, launchd labels, or
  keys at a provider. Write a migration script, or list it under "Deviations kept".
- **`tests/naming.rs` checks what it can, and runs in `scripts/check.sh`.** A name it rejects is
  renamed, not exempted.

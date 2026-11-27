Closes #

## What

<!-- One paragraph: what this PR does and which task card it implements. -->

## Ported Go tests

<!-- List the Go tests ported (Go name → Rust name). List dropped ones with a reason. -->

## Checklist (AGENTS.md §8)

- [ ] Every acceptance criterion of the card is met (tick them on the issue too)
- [ ] Stayed inside the card's Scope
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo nextest run --workspace` and `cargo test --workspace --doc`
- [ ] `cargo deny check`
- [ ] No `unwrap`/`expect`/`panic!`/`todo!` outside tests; no new dependency without a reason below
- [ ] Manual checks the card asks for are described below

## Notes for the reviewer

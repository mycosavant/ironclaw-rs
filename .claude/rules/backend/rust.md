---
paths:
  - "**/*.rs"
---

# Rust Idioms

Complementary to project conventions in CLAUDE.md. These are general Rust patterns to follow.

## Ownership & Borrowing

- Prefer `&T` over cloning unless ownership transfer is needed
- Use `&str` for function parameters when you don't need ownership
- Keep iterators lazy — avoid premature `collect()`
- Prefer zero-copy and borrowing over allocations

## Common Traits

Eagerly implement where appropriate: `Debug`, `Clone`, `PartialEq`, `Default`, `Display`. Use `From`/`AsRef` for conversions. Collections should implement `FromIterator` and `Extend`.

## API Design

- Use newtypes for static type distinctions (not raw strings or bools)
- Prefer specific types over generic `bool` parameters
- Struct fields should be private; expose through methods
- Use sealed traits when downstream implementations should be prevented

## Style

- Lines under 100 characters when practical
- `///` rustdoc on all public items; examples use `?` not `unwrap()`
- Algorithm-heavy code gets approach explanations in comments

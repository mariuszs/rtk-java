# Local Development Preferences

## Native CPU Installation

When installing rtk locally, always use native CPU target for maximum performance:

```bash
RUSTFLAGS="-C target-cpu=native" cargo install --path .
```

# Development

## Creating a Release

Releases are automated via GitHub Actions. Pushing a version tag triggers cross-platform builds and publishes binaries to GitHub Releases.

### Steps

1. Update the version in `Cargo.toml`:

   ```toml
   [package]
   version = "0.2.0"
   ```

2. Commit the version bump:

   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "Bump version to v0.2.0"
   ```

3. Tag and push:

   ```bash
   git tag v0.2.0
   git push origin master v0.2.0
   ```

The release workflow builds binaries for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

Once complete, the [GitHub Release](https://github.com/gcacace/coldpack/releases) will contain platform archives and a `checksums-sha256.txt` file.

### Verifying a Release

Download and check the checksum:

```bash
sha256sum -c checksums-sha256.txt --ignore-missing
```

Run the binary:

```bash
tar xzf coldpack-v0.2.0-x86_64-unknown-linux-gnu.tar.gz
./coldpack --help
```

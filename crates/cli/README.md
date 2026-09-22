# Cli

## Testing

You can test your changes to the `cli` crate by first building the main Sawe binary:

```
cargo build -p zed
```

And then building and running the `cli` crate with the following parameters:

```
 cargo run -p cli -- --zed ./target/debug/sawe.exe
```

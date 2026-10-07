# simple-http-upload

Tiny HTTP file-drop server with a web interface.

* Zero dependencies
* Upload files through a web interface
* Download files
* `curl` support
* Streaming uploads
* Upload progress display
* Prevents overwriting existing files
* Directory listing
* Bind to a specific interface
* Standard library only

## Installation

```bash
cargo install --git https://github.com/shoxxdj/simple-http-upload.git
```

## Usage

```bash
simple_http_upload <PORT>
```

For example:

```bash
simple_http_upload 8000
```

Then open:

```text
http://localhost:8000
```

By default, the current directory is served and uploaded files are stored there.

## Options

```text
-p, --port <PORT>         Port to listen on
-i, --interface <ADDR>    Interface to bind (default: 0.0.0.0)
-d, --dir <DIR>           Directory to serve and store uploads in (default: .)
    --no-listing          Disable directory listing
-h, --help                Show this help
```

The port can also be provided without an option:

```bash
simple_http_upload 8000
```

## Examples

Serve the current directory:

```bash
simple_http_upload 8000
```

Serve a specific directory:

```bash
simple_http_upload -d /tmp/uploads 8000
```

Listen only on localhost:

```bash
simple_http_upload -i 127.0.0.1 8000
```

Disable directory listing:

```bash
simple_http_upload --no-listing 8000
```

## Upload with curl

Files can be uploaded directly with `curl`:

```bash
curl -T file.txt http://localhost:8000/
```

Upload to a subdirectory:

```bash
curl -T file.txt http://localhost:8000/subdir/
```

The server uses HTTP `PUT` for uploads.

Existing files are never overwritten. If a file already exists, a new name is generated:

```text
file.txt
file (1).txt
file (2).txt
```

## Security

This tool has **no authentication**.

When listening on `0.0.0.0`, anyone who can reach the server can upload files.

Use `--interface 127.0.0.1` if the server should only be accessible locally.

The server also prevents path traversal and uploaded files are not served as executable HTML/JavaScript content.

## License

MIT


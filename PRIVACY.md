# Privacy policy

vibesurfer (`vs`) is a local command-line tool that drives a real browser engine on your machine. It does not collect, transmit, or store any data on remote servers. There is no telemetry, no analytics, and no usage reporting.

State is persisted only in the user's local `~/.vibesurfer/` directory: a SQLite database (`state.db`) for sessions, audit rows, marks, annotations, and auth blobs; an AF_UNIX socket (`daemon.sock`) the CLI talks to; PNG screenshots in `captures/`; and the optional master key file (`key`) used when no system keyring is available. Nothing is transmitted off the host.

The MCP server (`vs mcp`) communicates only with the local LLM client over stdio. The daemon (`vs serve`) listens only on a local Unix socket or Windows named pipe; it does not bind any network port.

The browser engines vibesurfer drives (WKWebView on macOS, WebKitGTK on Linux, WebView2 on Windows) make HTTP and WebSocket requests to whatever URLs the user (or the user's agent) navigates to. Those requests carry the host's normal browser identity and cookies; vibesurfer does not modify the User-Agent, override `navigator.webdriver`, or inject fingerprint-spoofing logic. Auth cookies and localStorage saved via `vs auth save` are encrypted at rest with AES-256-GCM using a key stored in the OS keyring (or `~/.vibesurfer/key` if no keyring is available).

The release tarballs and the Homebrew cask are downloaded from GitHub Releases on first install; subsequent runs are fully offline.

Contact: frane.bandov@gmail.com
Source: https://github.com/frane/vibesurfer

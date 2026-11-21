# Changelog

Each release's section here becomes its notes on GitHub, and what the update
dialog shows. Add a `## x.y.z` section before tagging `vx.y.z`.

## 0.1.0

- First release: files on S3, SFTP and FTP (explicit FTPS too), SSH and local
  terminals, and this computer's own files, in tabs, with two views side
  by side
- S3: buckets and their settings (versioning, lifecycle, CORS, policy,
  public access), object versions, metadata, sharing links; path-style
  addressing for buckets with dots in their names (Wasabi, MinIO and the
  like)
- Transfers that resume, with speed limits, a number running at once,
  rules for files that exist already, pausing, and folder synchronisation
- Copies between the two views, from any connection to any other; drag
  and drop with the file manager, both ways
- A built-in text editor with syntax colours, encodings and line endings,
  saving straight back to the server; or any local program, the file sent
  back when it is saved there
- A quick preview of files without downloading them, a search below a
  folder by name, kind, size and date, and logs followed as they grow
- Rename, move and copy on the server, permissions and owner, folder sizes
  and bookmarks
- SSH: agents (Windows' OpenSSH agent, 1Password, Bitwarden, Pageant), key
  files, a keychain of identities shared by connections, new keys made and
  installed on servers, jump hosts, port forwarding (-L, -R, -D)
- Terminals with search, links, session logs, saved commands, typing into
  several at once, and a live overview of the server
- Connections in folders, with favourites, colours and tags, Ctrl+K to
  open one by name, an encrypted export, and an import from FileZilla,
  Windows Terminal and ~/.ssh/config; the tabs open at closing come back at
  the next start
- Windows installer that keeps Farfile up to date, and portable builds for
  Windows and Linux that ask before updating; downloads are checked against
  their SHA-256

# Changelog

Each release's section here becomes its notes on GitHub, and what the update
dialog shows. Add a `## x.y.z` section before tagging `vx.y.z`.

## 0.4.0

- Monitoring: history kept while Farfile is closed, for the servers you
  choose (**Historique hors connexion…** in the menu of a server, or on its
  page). A short script, run by cron once a minute, writes one line on the
  processor, memory, load and network of the server, and its disks every
  quarter of an hour, in `~/.local/share/farfile` for eight days at most.
  Farfile reads them back when it connects again, and the charts have no
  gap. Nothing stays running and nothing is sent: about 2 ms of processor a
  minute, 100 KB a day. No administrator rights needed; the line it adds to
  the crontab goes with it when it is removed, the other lines untouched.
  Updated by itself when a new version or new disks need it

## 0.3.0

- Monitoring: five views of the dashboard, chosen at its top and kept for
  next time. Cards made of the widgets you pick, in the order and style you
  pick (gauge, curve, ring, figure), with ready-made sets; a sortable list,
  grouped by folder or state; a heat map of every server over time; every
  server on the same chart, with a small chart each underneath; and a wall
  of large tiles for a screen of its own, in full screen if wanted
- A click on a server shows it in full: processor, memory, network and
  load as charts with axes, every disk with when it will be full at the
  pace it fills, and its alerts
- Charts over 30 minutes, 6 hours, 24 hours or 7 days: what the watched
  servers did is kept on disk, so it survives a restart
- Folder sizes on demand: a button above the files measures every folder
  shown, its size filling in as it comes, and can be stopped; a folder that
  could not be read is said, as the real size may then be larger
- Folders deleted over SFTP go in one command on the server (`rm -rf`)
  instead of a request per file: a thousand files in a tenth of a second
  rather than forty seconds on a distant server. Accounts limited to SFTP
  still delete file by file
- Fixed: deleting a link to a folder over SFTP emptied the folder it
  points to. Now only the link goes

## 0.2.0

- The interface speaks English, French, German and Spanish: the system's
  language by default, or the one chosen from the globe at the foot of the
  list of connections; sizes, decimals and dates follow it. Copies already
  in use stay in French until another language is chosen
- The installer speaks them too
- Monitoring: a dashboard of several SSH servers at once (processor,
  memory, disks, load, network, the last half hour as curves), opened
  from **Surveillance** under "Cet ordinateur". Alerts when a disk, the
  memory, the processor, the swap or the load crosses its threshold
  (85 % to warn and 95 % critical by default, some only after lasting a
  few minutes), or when a server stops answering; thresholds for all
  servers or for one, told by a message in the window, a Windows
  notification and the taskbar button, as chosen. Closing the window can
  keep watching from the notification area. A login the server refuses
  pauses its watch rather than trying again

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

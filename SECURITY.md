# Security

Please don't report security problems in public issues.

Report them privately through GitHub instead: on the
[Security tab](https://github.com/Koimsurai/Arke/security) choose **Report a vulnerability**.
Include the Arke version (Settings › About), your Windows version and the steps to reproduce.
You'll get an answer within a few days, and a fix in a release as soon as possible.

Only the latest release is supported.

## What Arke does that matters for security

- It sends no telemetry. Besides installing the optional search service (with your approval;
  the Service Control Manager records it under
  `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\ArkeIndex`), it writes to the registry
  only when you turn on "Open folders in Arke": then it registers itself as the folder handler under `HKEY_CURRENT_USER\Software\Classes`
  (the current user only, no administrator rights), backs up what was there, and restores it
  when you turn the setting off or run `arke --restore-explorer`. It contacts GitHub only to
  check for updates, at most once a day, and only installs one when you say so.
- Shell extensions from other programs run in a separate `--shell-host` process, never in Arke.
- The optional search service runs as a Windows service with administrator rights. It reads the
  file system's master file table and change journal, never file contents, and answers only the
  user who is signed in, over a local named pipe.
- Passwords for SFTP and WebDAV locations are stored in Windows Credential Manager, and only
  when you tick "Remember". SFTP host keys are checked against `%APPDATA%\Arke\known_hosts`.
- Releases come with a SHA-256 checksum. The builds aren't code-signed yet.

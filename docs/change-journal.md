# The NTFS change journal and Arke

Every NTFS drive keeps a change journal (the USN journal): a running list of the files and folders
that were created, renamed, changed or deleted on it. Windows keeps it, not Arke, and one journal is
shared by every program that reads it, such as Windows Search, backup tools and Everything.

## How Arke uses it

Only with the optional search service (Settings › Search › Live updates (background service)). Arke stores how far it
had read along with its index; when it starts again, it reads what changed while it was closed and
the index is current at once. Without the service, Arke doesn't read the journal at all and keeps
the index current with folder watching instead.

Arke only reads the journal. It never changes its size and never deletes it.

## When the journal doesn't reach back far enough

The journal has a maximum size, 32 MB by default on most drives: roughly a day of normal use, a few
hours on a very busy drive. Once it is full, the oldest entries are dropped. If Arke was closed
longer than that, or the journal was deleted or replaced, Arke can't tell what changed and rebuilds
that index instead: with the search service it reads the drive's file table directly, which takes
about 10 to 40 seconds for a few million files. The previous index keeps answering searches
meanwhile. This is expected and not an error; the log says why it happened
(`global search: rebuilding C:\: ...`).

Leaving Arke running in the tray (Settings › Startup › Keep running after closing windows) means it
keeps reading as changes happen, so this rarely comes up.

## Why Arke no longer offers to make the journal larger

Earlier versions let you raise the journal size in Settings. Windows can make an existing journal
larger, but it never makes one smaller again: asked for a smaller size, it keeps the current one
and still reports success. This was checked on Windows 11 by asking for 32 MB on a 128 MB journal,
waiting, filling the journal until Windows trimmed it, and remounting the drive; the journal stayed
at its larger size every time (see [#150](https://github.com/Koimsurai/Arke/issues/150)).

Since the journal is shared, a larger one is a change to your system that other programs see too,
and since rebuilding Arke's index takes seconds, Arke doesn't need it.

## Going back to the default size

A larger journal does no harm besides the disk space it takes. If you want that space back, the
only way is to delete the journal and let a new one be created, which needs an administrator
command prompt:

```bat
fsutil usn queryjournal C:
fsutil usn deletejournal /n C:
```

On the system drive (usually `C:`), Windows creates a new journal of the default size within a few
seconds. On other drives, create it yourself; `m=0 a=0` means "the default size for this drive":

```bat
fsutil usn createjournal m=0 a=0 D:
```

What this does to other programs: every program that reads the journal sees a new one and has to
look through the drive again to know what changed. Windows Search re-indexes the folders it
covers, backup tools that use the journal may compare files on their next run instead of using the
journal, and Everything and Arke rebuild their indexes. Your files and settings are not touched.
Don't do this on a server that uses DFS Replication: it resynchronizes the replicated folders.

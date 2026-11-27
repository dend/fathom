//! What `fth help` shows.

use super::args::Command;

pub const OVERVIEW: &str = "\
fth: Fathom's command line tool for Cloudflare R2

Usage: fth <command> [options]

Commands:
  ls [<bucket> [<pattern>...]]     List buckets, the top of a bucket, or the
                                   objects that patterns pick
  upload <bucket> <path>...        Upload files and folders
  download <bucket> <pattern>...   Download objects
  delete <bucket> <pattern>...     Delete objects
  purge <bucket> <pattern>...      Purge objects from the cache of the bucket's
                                   custom domains
  connections                      List the connections saved in Fathom
  update                           Update Fathom and fth to the newest release
  help [<command>]                 Show the options and examples of a command

Patterns pick objects the same way in every command:
  photos/reef.png      that object, or everything in a folder of that name
  photos/              everything in the photos folder
  photos/*.png         * matches any part of a name, ? one character, [abc]
                       one of those characters and [!abc] any other
  photos/**            ** matches across folders
  logs/2024-**         everything whose key starts with logs/2024-
  **/*.{jpg,png}       {a,b} matches either
With --regex, patterns are regular expressions instead. Quote patterns so the
shell passes them on as they are.

Options:
  -c, --connection <name>  Use this saved connection instead of the one last
                           open in Fathom
  -E, --regex              Treat patterns as regular expressions, matched
                           anywhere in the key unless anchored with ^ or $
  -n, --dry-run            Show what would happen, without doing it
  -h, --help               Show help
  -V, --version            Show the version

Credentials:
  fth uses the connections saved in Fathom. Where there are none, as in CI,
  set CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN. These are used when set,
  unless --connection picks a saved connection.
";

pub fn command(command: Command) -> &'static str {
    match command {
        Command::Ls => LS,
        Command::Upload => UPLOAD,
        Command::Download => DOWNLOAD,
        Command::Delete => DELETE,
        Command::Purge => PURGE,
        Command::Connections => CONNECTIONS,
        Command::Update => UPDATE,
    }
}

const LS: &str = "\
Usage: fth ls [<bucket> [<pattern>...]] [options]

Without a bucket, lists the buckets. Without a pattern, lists the folders and
files at the top of the bucket. With patterns, lists every object they pick:
what download, delete or purge would act on with the same patterns.

Options:
  -l, --long               Show sizes and dates
  -E, --regex              Treat patterns as regular expressions
  -c, --connection <name>  Use this saved connection

Examples:
  fth ls
  fth ls media 'photos/**/*.png' --long
  fth ls media --regex '^logs/2024-0[1-6]/'
";

const UPLOAD: &str = "\
Usage: fth upload <bucket> <path>... [options]

Uploads files and folders, replacing objects that are already there. A folder
keeps its name, so uploading dist creates dist/index.html. A pattern keeps the
part of each path after the folders it starts with, so 'dist/**' uploads
dist/index.html as index.html, and 'dist/css/*.css' uploads dist/css/site.css
as site.css.

Options:
  --to <folder>            Upload into this folder of the bucket
  --purge                  Then purge the uploaded objects from the cache of
                           the bucket's custom domains
  -E, --regex              Treat paths as regular expressions, matched against
                           paths below the current folder, written with /
  -n, --dry-run            Show what would be uploaded
  -c, --connection <name>  Use this saved connection

Examples:
  fth upload media photo.jpg --to photos/2024/
  fth upload website 'dist/**' --purge
  fth upload backups --regex '^exports/.*\\.csv$'
";

const DOWNLOAD: &str = "\
Usage: fth download <bucket> <pattern>... [options]

Downloads the objects that patterns pick, replacing files that are already
there. A file or folder named in full keeps its name. A wildcard pattern keeps
the part of each key after the folders it starts with, so 'photos/2024/*.png'
saves photos/2024/reef.png as reef.png.

Options:
  --to <folder>            Save into this folder instead of the current one
  -E, --regex              Treat patterns as regular expressions
  -n, --dry-run            Show what would be downloaded
  -c, --connection <name>  Use this saved connection

Examples:
  fth download media photos/2024/ --to backup
  fth download media 'photos/**/*.{jpg,png}'
  fth download logs --regex '^app/2024-11-2[0-9]/' --to logs
";

const DELETE: &str = "\
Usage: fth delete <bucket> <pattern>... [options]

Deletes the objects that patterns pick. It shows them and asks first, unless
--yes is given. Run it with --dry-run, or run ls with the same patterns, to see
what they pick.

Options:
  -y, --yes                Delete without asking
  --purge                  Then purge the deleted objects from the cache of the
                           bucket's custom domains
  -E, --regex              Treat patterns as regular expressions
  -n, --dry-run            Show what would be deleted
  -c, --connection <name>  Use this saved connection

Examples:
  fth delete media photos/old/
  fth delete logs 'app/2023-**' --yes
  fth delete media --regex '\\.(tmp|bak)$'
";

const PURGE: &str = "\
Usage: fth purge <bucket> <pattern>... [options]

Purges objects from the Cloudflare cache of every custom domain of the bucket,
so the next request gets them from R2 again. A file named in full is purged even
if it was deleted. A folder ending in / is purged as a whole, deleted files and
all. A wildcard pattern purges the objects it picks in the bucket.

This needs a connection set up with an API token that has the
Zone > Cache Purge permission for the domains' zones.

Options:
  -E, --regex              Treat patterns as regular expressions
  -n, --dry-run            Show what would be purged
  -c, --connection <name>  Use this saved connection

Examples:
  fth purge website index.html css/site.css
  fth purge media photos/
  fth purge website '**/*.html'
";

const CONNECTIONS: &str = "\
Usage: fth connections

Lists the connections saved in Fathom, marking the one commands use unless
--connection names another, by its name or its ID.
";

const UPDATE: &str = "\
Usage: fth update [--check]

Downloads the newest release of Fathom from GitHub, checks it against the
release's checksums and installs it, which updates the app and fth together.

In a terminal, other commands mention a newer release when there is one,
checking at most once a day. Set FATHOM_NO_UPDATE_CHECK to turn that off.

Options:
  --check                  Only say whether there's a newer release
";

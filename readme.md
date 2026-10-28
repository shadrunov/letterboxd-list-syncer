# <img src="https://a.ltrbxd.com/logos/letterboxd-decal-dots-pos-rgb.svg" alt="Letterboxd logo" width="28"> letterboxd list syncer

A small Rust service that mirrors a public Letterboxd list into a list owned by
your account. GitHub Actions runs it every six hours. Changes in the source are copied to the target.

## configuration

Edit [`config.yaml`](config.yaml):

```yaml
username: shadrunov

lists:
  - source: https://letterboxd.com/cokewithglitter/list/aa-vampire-season/
    target: "a&a: vampire season"

tagged_lists:
  - tag: w/ andrei
    target: andrew
```

Set `username` once, then add a `source` URL and target list name for every list
you want to mirror. The target URL is derived from the username and name. A
missing target is created on the first successful run. Later runs find it by
name, so Letterboxd-generated slug suffixes do not create duplicates.

Mirrored lists are assumed to be public and unranked. The source description is copied with links to the source and this repository. If the source
has no description, this credit is the complete target description.

`tagged_lists` incrementally maintains a list whose watched prefix follows a
diary tag. Each run reads only the first page of that tag's diary. New tagged
films are inserted oldest-to-newest immediately after the last contiguous
watched film; the unwatched tail keeps its order. If a newly watched film is
already in that tail, it is moved to the watched boundary instead of being
duplicated. The sync fails without changing the list if the recent diary does
not provide a reliable overlap with the existing watched prefix.

## authentication

Letterboxd's official API is request-only, and its access page says personal
projects are not currently granted access. This service therefore uses your
revocable “remember me” web session, never your password.

Log in to Letterboxd in a desktop browser with **Remember me** enabled. Open
Developer Tools, select **Application** (Chrome) or **Storage** (Firefox), open
Cookies for `https://letterboxd.com`, and copy the values of:

- `letterboxd.user`
- `com.xk72.webparts.csrf`

In the GitHub repository, open **Settings → Secrets and variables → Actions**
and create these repository secrets:

- `LETTERBOXD_USER_COOKIE`
- `LETTERBOXD_CSRF_COOKIE`

Treat both values like a password. Do not paste them into an issue, commit, or
workflow file. Logging out of Letterboxd revokes the session; update both
secrets when it expires.

## run

```sh
cargo run -- --dry-run
cargo run
```

The dry run only reads public lists and tagged diary pages. The normal run uses
the session cookies to create or update lists owned by `username`.

GitHub Actions runs every six hours at minute `17` (UTC) and can also be started
from the Actions tab. Concurrent runs are serialized.

## caveat

Letterboxd does not publish a supported personal automation API. The
authenticated write path can change without notice. CI tests protect the
parser and configuration, while every sync fails closed on authentication,
ownership, version, or API errors.

## authorship

This repository and all of its code were co-authored by Aleksei Shadrunov and
Codex by OpenAI.

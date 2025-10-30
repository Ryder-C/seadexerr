# Seadexerr

A Prowlarr indexer for [Seadex](https://releases.moe/) torrents. Always get the best Seadex release.

> [!NOTE]
> Requires indexer flag `internal` to be unused for now

## Docker Compose

```yaml
services:
  seadexerr:
    image: ghcr.io/ryder-c/seadexerr:main
    container_name: seadexerr
    environment:
      - SONARR_BASE_URL=http://localhost:8989/
      - SONARR_API_KEY=<your api key here>
```

<details>
<summary>Advanced Configuration</summary>
Most can be left as default

| Variable                         | Default                                                                                          | Purpose                                                                           |
| -------------------------------- | ------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------- |
| `SONARR_API_KEY`                 | **(required)**                                                                                   | Sonarr API key used to resolve series titles.                                     |
| `SONARR_BASE_URL`                | `http://localhost:8989/`                                                                         | Base URL for your Sonarr instance.                                                |
| `SEADEXER_HOST`                  | `0.0.0.0`                                                                                        | Interface the HTTP server listens on.                                             |
| `SEADEXER_PORT`                  | `6767`                                                                                           | TCP port Seadexerr binds to. Must be a valid `u16`.                               |
| `SEADEXER_PUBLIC_BASE_URL`       | (optional; falls back to `http://{SEADEXER_HOST}:{SEADEXER_PORT}`)                               | Base URL advertised in the Torznab feed. Set when running behind a reverse proxy. |
| `SEADEXER_TITLE`                 | `Seadexerr`                                                                                      | Channel title reported to Torznab clients.                                        |
| `SEADEXER_DESCRIPTION`           | `Indexer bridge for releases.moe`                                                                | Channel description shown to Torznab clients.                                     |
| `SEADEXER_DEFAULT_LIMIT`         | `100`                                                                                            | Maximum number of results returned in a single Torznab feed.                      |
| `SEADEXER_RELEASES_BASE_URL`     | `https://releases.moe/api/`                                                                      | Root URL for the releases.moe API.                                                |
| `SEADEXER_RELEASES_TIMEOUT_SECS` | `10`                                                                                             | Timeout (seconds) for releases.moe requests.                                      |
| `SEADEXER_DATA_PATH`             | `data`                                                                                           | Directory used to store downloaded data, including mapping files.                 |
| `SEADEXER_MAPPING_SOURCE_URL`    | `https://raw.githubusercontent.com/eliasbenb/PlexAniBridge-Mappings/refs/heads/v2/mappings.json` | URL to the PlexAniBridge mappings JSON.                                           |
| `SEADEXER_MAPPING_REFRESH_SECS`  | `21600`                                                                                          | Interval (seconds) between background mapping refreshes.                          |
| `SEADEXER_MAPPING_TIMEOUT_SECS`  | `SEADEXER_RELEASES_TIMEOUT_SECS` (10)                                                            | Timeout (seconds) for PlexAniBridge downloads.                                    |
| `SONARR_TIMEOUT_SECS`            | `10`                                                                                             | Timeout (seconds) for Sonarr API requests.                                        |

</details>

## Prowlarr & Sonarr Integration

In Prowlarr:

1. Click on **Add Indexer**
2. Search for **Generic Torznab** and click it
3. Change **Name** to `Seadexerr`
4. Set **Url** to `http://seadexerr:6767`
5. Click **Test** and **Save**

In Sonarr:

1. Go to **Settings → Custom Formats**
2. Click to create a new **Custom Format**
3. Set **Name** to `Seadex`
4. Add an **Indexer Flag Condition**.
5. Set **Name** and **Flag** to `Internal` (leave boxes unchecked)
6. Click **Test** and **Save**

## Future Plans

- [ ] RSS Refresh
- [ ] Movie Support
- [ ] Specials Support
- [x] Local PlexAniBridge Mappings

This project uses [PlexAniBridge Mappings](https://github.com/eliasbenb/PlexAniBridge-Mappings).

Contributions and feature suggestions are welcome. Open an issue or submit a pull request to get involved.

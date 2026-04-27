# Seadexerr

A Prowlarr indexer for [Seadex](https://releases.moe/) torrents. Always get the best Seadex release.

> [!NOTE]
> Automatic Searching requires indexer flag `Freeleech25` to be unused for now

## Docker Compose

```yaml
services:
  seadexerr:
    image: ghcr.io/ryder-c/seadexerr:latest
    container_name: seadexerr
    environment:
      - SONARR_BASE_URL=http://localhost:8989/
      - SONARR_API_KEY=<your api key here>
      - RADARR_BASE_URL=http://localhost:7878/
      - RADARR_API_KEY=<your api key here>
```

<details>
<summary>Advanced Configuration</summary>
Most can be left as default

| Variable                      | Default                                                              | Purpose                                                                           |
| ----------------------------- | -------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `SONARR_API_KEY`              | (optional\*)                                                         | Sonarr API key used to resolve series titles. Required if using Sonarr.           |
| `SONARR_BASE_URL`             | `http://localhost:8989/`                                             | Base URL for your Sonarr instance.                                                |
| `RADARR_API_KEY`              | (optional\*)                                                         | Radarr API key used to resolve movie titles. Required if using Radarr.            |
| `RADARR_BASE_URL`             | `http://localhost:7878/`                                             | Base URL for your Radarr instance.                                                |
| `SEADEXERR_HOST`              | `0.0.0.0`                                                            | Interface the HTTP server listens on.                                             |
| `SEADEXERR_PORT`              | `6767`                                                               | TCP port Seadexerr binds to. Must be a valid `u16`.                               |
| `SEADEXERR_PUBLIC_BASE_URL`   | (optional; falls back to `http://{SEADEXERR_HOST}:{SEADEXERR_PORT}`) | Base URL advertised in the Torznab feed. Set when running behind a reverse proxy. |
| `SEADEXERR_SKIP_DEBAND`       | `false`                                                              | Skip releases with the `Deband Required` tag.                                     |
| `SEADEXERR_PREFER_DUAL_AUDIO` | `false`                                                              | Prefer dual audio releases when multiple options are available.                   |

\* At least one of `SONARR_API_KEY` or `RADARR_API_KEY` must be provided. If only one is provided, the other service is disabled.

</details>

## Prowlarr & Sonarr Integration

In Prowlarr:

1. Click on **Add Indexer**
2. Search for **Generic Torznab** and click it
3. Change **Name** to `Seadexerr`
4. Set **Url** to `http://seadexerr:6767`
5. Click **Test** and **Save**

In Sonarr or Radarr:

1. Go to **Settings → Custom Formats**
2. Create a new **Custom Format** named `Seadex`
3. Add an **Indexer Flag Condition**
4. Set both **Name** and **Flag** to `Freeleech25` (leave boxes unchecked)
5. Click **Test** and **Save**
6. Go to **Settings → Profiles**
7. Click on your profile and give a high score to Seadex (Ex: 5000)

This project uses [AniBridge Mappings](https://github.com/anibridge/anibridge-mappings).

Contributions and feature suggestions are welcome. Open an issue or submit a pull request to get involved.

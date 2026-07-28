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
    volumes:
      - ./data:/data
```

<details>
<summary>Advanced Configuration</summary>
Most can be left as default

| Variable                    | Default                                                              | Purpose                                                                                                      |
| --------------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `SONARR_API_KEY`            | (optional\*)                                                         | Sonarr API key used to resolve series titles. Required if using Sonarr.                                      |
| `SONARR_BASE_URL`           | `http://localhost:8989/`                                             | Base URL for your Sonarr instance.                                                                           |
| `RADARR_API_KEY`            | (optional\*)                                                         | Radarr API key used to resolve movie titles. Required if using Radarr.                                       |
| `RADARR_BASE_URL`           | `http://localhost:7878/`                                             | Base URL for your Radarr instance.                                                                           |
| `SEADEXERR_HOST`            | `0.0.0.0`                                                            | Interface the HTTP server listens on.                                                                        |
| `SEADEXERR_PORT`            | `6767`                                                               | TCP port Seadexerr binds to. Must be a valid `u16`.                                                          |
| `SEADEXERR_PUBLIC_BASE_URL` | (optional; falls back to `http://{SEADEXERR_HOST}:{SEADEXERR_PORT}`) | Base URL advertised in the Torznab feed. Set when running behind a reverse proxy.                            |
| `SEADEXERR_SKIP_DEBAND`     | `false`                                                              | **Deprecated** - use `exclude_tags` in `scoring.toml`. Skip releases with the `Deband Required` tag.         |
| `SEADEXERR_PREFER`          | `best`                                                               | **Deprecated** - use `scoring.toml`. Prefer `best`, `dual_audio`, or `smallest` when multiple options exist. |
| `AB_PASSKEY`                | (optional)                                                           | AnimeBytes passkey. See [AnimeBytes Support](#animebytes-support).                                           |

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

<details>
<summary>Setting up the custom format with recyclarr</summary>

If [recyclarr](https://recyclarr.dev) manages your Sonarr/Radarr, you can define the custom format there instead of clicking it together in the UI, so it lives in your config with the rest of your setup.

Save this next to your recyclarr config, for example as `custom-formats/sonarr/seadex.json`. For Radarr, change `"value": 64` to `512` (the two apps number their indexer flags differently).

```json
{
  "trash_id": "seadex",
  "name": "Seadex",
  "includeCustomFormatWhenRenaming": false,
  "specifications": [
    {
      "name": "Freeleech25",
      "implementation": "IndexerFlagSpecification",
      "negate": false,
      "required": false,
      "fields": { "value": 64 }
    }
  ]
}
```

Point recyclarr at that folder in `settings.yml` (needs recyclarr 7.5.2 or newer):

```yaml
resource_providers:
  - name: local-cfs-sonarr
    type: custom-formats
    path: /config/custom-formats/sonarr
    service: sonarr
```

Then score it in your config like any other custom format:

```yaml
custom_formats:
  - trash_ids:
      - seadex
    assign_scores_to:
      - name: <your profile>
        score: 5000
```

</details>

## Release Scoring

When several releases exist for the same entry, Seadexerr picks the one you'd
prefer. By default it favors the Best release. To change what wins, make a
`scoring.toml` in your data directory.

See [`example_scoring.toml`](example_scoring.toml) for a fully commented file.
Every option has a default, so keep only what you care about.

## AnimeBytes Support

Seadexerr can also serve AnimeBytes releases listed on Seadex. Set
`AB_PASSKEY` to your AnimeBytes passkey to enable it.

> [!NOTE]
> I rely on issue reports to find and fix AnimeBytes-specific breakage.

This project uses [AniBridge Mappings](https://github.com/anibridge/anibridge-mappings).

Contributions and feature suggestions are welcome. Open an issue or submit a pull request to get involved.

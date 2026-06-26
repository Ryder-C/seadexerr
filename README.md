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
| `AB_PASSKEY`                | (optional)                                                           | AnimeBytes passkey. Enables AnimeBytes releases. See [AnimeBytes Support](#animebytes-support).              |

\* At least one of `SONARR_API_KEY` or `RADARR_API_KEY` must be provided. If only one is provided, the other service is disabled.

</details>

## AnimeBytes Support

Seadexerr can also serve AnimeBytes releases listed on Seadex. Set
`AB_PASSKEY` to your AnimeBytes passkey to enable it.

> [!NOTE]
> I rely on issue reports to find and fix AnimeBytes-specific breakage.

## Release Scoring

When several releases exist for the same entry, Seadexerr scores each one and
advertises the highest scorer(s) to Sonarr/Radarr with a boosted seeder count so
they win the grab. Scoring is configured in a `scoring.toml` file placed in the
data directory (mount a volume to `/data`):

```yaml
services:
  seadexerr:
    image: ghcr.io/ryder-c/seadexerr:latest
    container_name: seadexerr
    environment:
      - SONARR_BASE_URL=http://localhost:8989/
      - SONARR_API_KEY=<your api key here>
    volumes:
      - ./data:/data
```

```toml
# data/scoring.toml
#
# Each release's score is the sum of the weights below for everything it
# matches. The highest scorer(s) get the seeder boost; ties all win. Weights
# are integers and may be negative (penalties). Unlisted tags score 0.

# Category weights (releases.moe's per-release booleans)
best = 100          # release marked "Best" on releases.moe
dual_audio = 50     # release marked Dual Audio

# Size axis. The release size is normalized across the candidates to [0, 1]
# (smallest = 1.0, largest = 0.0) and multiplied by this weight.
#   > 0  favors smaller releases
#   < 0  favors larger releases
#   0    ignores size entirely
size_weight = 30

# Tags removed from results entirely (not just deprioritized). Match the exact
# releases.moe label. Replaces the deprecated SEADEXERR_SKIP_DEBAND.
exclude_tags = ["Deband Required"]

# Per-tag weights, keyed by the exact releases.moe tag label.
[tags]
"HDR"                = 20
"Dolby Vision"       = 20
"YUV444P"            = 10
"Deband Recommended" = 5
"VFR"                = -10
"Misplaced Special"  = -20
"Patch Required"     = -30
"Broken"             = -1000
```

Every field has a default, so a minimal file only needs what you care about:

```toml
best = 100

[tags]
"HDR" = 20
```

If no `scoring.toml` is present, Seadexerr defaults to preferring the Best
release. The deprecated `SEADEXERR_PREFER` / `SEADEXERR_SKIP_DEBAND` variables
still work as a fallback, but log a warning when set.

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

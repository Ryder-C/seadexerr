{
  description = "Seadexerr development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            sonarr
            radarr
            prowlarr

            # Rust development tools
            cargo
            rustc
            rustfmt
            clippy

            # Common dependencies
            pkg-config
            openssl
          ];

          shellHook = ''
            echo "Seadexerr development environment loaded."

            export SEADEXERR_DEV_ROOT="$PWD/.dev-env"
            mkdir -p "$SEADEXERR_DEV_ROOT"

            # Create media directories for Sonarr/Radarr root paths
            export RADARR_ROOT_PATH="$SEADEXERR_DEV_ROOT/media/movies"
            export SONARR_ROOT_PATH="$SEADEXERR_DEV_ROOT/media/tv"
            mkdir -p "$RADARR_ROOT_PATH"
            mkdir -p "$SONARR_ROOT_PATH"

            start_service() {
                local name=$1
                local cmd=$2
                local data_dir="$SEADEXERR_DEV_ROOT/$name"
                mkdir -p "$data_dir"

                # Try to find the executable, fallback to name
                if command -v $name &> /dev/null; then
                    EXEC=$name
                elif command -v $cmd &> /dev/null; then
                    EXEC=$cmd
                else
                    echo "Could not find executable for $name"
                    return
                fi

                echo "Starting $name..."
                $EXEC --data="$data_dir" --nobrowser > "$data_dir/$name.log" 2>&1 &
                echo $!
            }

            # Start services and capture PIDs
            SONARR_PID=$(start_service "sonarr" "Sonarr")
            RADARR_PID=$(start_service "radarr" "Radarr")
            PROWLARR_PID=$(start_service "prowlarr" "Prowlarr")

            # Function to extract API key from config.xml
            get_api_key() {
                local name=$1
                local config_file="$SEADEXERR_DEV_ROOT/$name/config.xml"
                local max_retries=30
                local count=0

                echo "Waiting for $name to generate config..." >&2
                while [ ! -f "$config_file" ] || ! grep -q "<ApiKey>" "$config_file"; do
                    sleep 1
                    count=$((count+1))
                    if [ $count -ge $max_retries ]; then
                        echo "Timeout waiting for $name config" >&2
                        return
                    fi
                done

                # Extract key between tags
                grep -o "<ApiKey>.*</ApiKey>" "$config_file" | sed -e 's/<[^>]*>//g'
            }

            # Export keys to environment
            export SONARR_API_KEY=$(get_api_key "sonarr")
            export RADARR_API_KEY=$(get_api_key "radarr")
            export PROWLARR_API_KEY=$(get_api_key "prowlarr")

            echo "----------------------------------------"
            echo "API Keys loaded:"
            echo "SONARR_API_KEY: $SONARR_API_KEY"
            echo "RADARR_API_KEY: $RADARR_API_KEY"
            echo "PROWLARR_API_KEY: $PROWLARR_API_KEY"
            echo "----------------------------------------"

            cleanup() {
                echo "Stopping background services..."
                [ -n "$SONARR_PID" ] && kill $SONARR_PID 2>/dev/null
                [ -n "$RADARR_PID" ] && kill $RADARR_PID 2>/dev/null
                [ -n "$PROWLARR_PID" ] && kill $PROWLARR_PID 2>/dev/null
            }

            trap cleanup EXIT

            echo "Services running in background. Data stored in .dev-env/"
          '';
        };
      }
    );
}

FROM rust:1.90.0-slim

ARG APP_USER=appuser
ARG APP_UID=1000
ARG APP_GID=1000
ARG APP_HOME=/app
ARG APP_DATA_PATH=data

ENV APP_HOME=${APP_HOME}
ENV HOME=${APP_HOME}
ENV SEADEXER_DATA_PATH=${APP_DATA_PATH}

RUN set -eux; \
    if ! getent group "${APP_GID}" >/dev/null; then \
        groupadd -g "${APP_GID}" "${APP_USER}"; \
    fi; \
    if ! id -u "${APP_USER}" >/dev/null 2>&1; then \
        useradd -m -d "${APP_HOME}" -u "${APP_UID}" -g "${APP_GID}" "${APP_USER}"; \
    fi

WORKDIR ${APP_HOME}
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo install --path .

RUN set -eux; \
    data_path="${APP_DATA_PATH:-data}"; \
    case "${data_path}" in \
        /*) resolved_data_path="${data_path}" ;; \
        *) resolved_data_path="${APP_HOME}/${data_path}" ;; \
    esac; \
    if [ "${resolved_data_path}" = "/" ]; then \
        echo "APP_DATA_PATH must not resolve to /" >&2; \
        exit 1; \
    fi; \
    mkdir -p "${resolved_data_path}"; \
    chown -R "${APP_UID}:${APP_GID}" "${APP_HOME}"; \
    if [ "${resolved_data_path}" != "${APP_HOME}" ]; then \
        chown -R "${APP_UID}:${APP_GID}" "${resolved_data_path}"; \
    fi

USER ${APP_USER}

CMD ["seadexerr"]

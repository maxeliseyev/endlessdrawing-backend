# Disposable LOCAL/CI test tools, never the application runtime or the production backup key.
FROM docker:cli
RUN apk add --no-cache bash coreutils util-linux openssl age python3

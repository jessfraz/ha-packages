#!/usr/bin/env bash
set -euo pipefail
umask 077

image=${1:?Usage: container-smoke.sh IMAGE EXPECTED_ARCH}
expected_arch=${2:?Usage: container-smoke.sh IMAGE EXPECTED_ARCH}
actual_arch=$(docker image inspect "$image" --format '{{.Architecture}}')
if [[ $actual_arch != "$expected_arch" ]]; then
    echo "Expected $expected_arch image, got $actual_arch" >&2
    exit 1
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/ha-packages-smoke.XXXXXX")
container="ha-packages-smoke-${RANDOM}-${RANDOM}"
volume="$container-data"
restore_volume="$container-restored"
outside_volume="$container-outside"
boundary_container="$container-boundary"
root_link_container="$container-root-link"
# Deliberately fake: this token exists only in the disposable local container.
token='container-smoke-only-no-provider-credentials'

cleanup() {
    status=$?
    if [[ $status -ne 0 ]]; then
        docker logs "$container" >&2 || true
    fi
    for resource in "$container" "$boundary_container" "$root_link_container"; do
        docker rm --force "$resource" >/dev/null 2>&1 || true
    done
    for resource in "$volume" "$restore_volume" "$outside_volume"; do
        docker volume rm "$resource" >/dev/null 2>&1 || true
    done
    rm -rf "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Export through the real Store admission path and checkpoint before copying.
PACKAGES_CONTAINER_FIXTURE="$scratch/packages.sqlite3" \
    cargo test --locked --test container_fixture export_container_fixture -- --ignored --exact
docker volume create "$volume" >/dev/null
docker create --name "$container" \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$volume,target=/data" \
    --env "PACKAGES_API_TOKEN=$token" \
    "$image" >/dev/null
docker cp "$scratch/packages.sqlite3" "$container:/data/packages.sqlite3"
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"

wait_for_api() {
    for _ in {1..60}; do
        if curl --fail --silent --max-time 1 \
            --header "Authorization: Bearer $token" \
            "$base_url/v1/system" --output "$scratch/system.json"; then
            return
        fi
        sleep 0.25
    done
    echo 'Container API did not become ready' >&2
    return 1
}

request() {
    local method=$1 path=$2 expected=$3 actual
    shift 3
    actual=$(curl --silent --show-error --max-time 5 \
        --request "$method" --output "$scratch/response.json" \
        --write-out '%{http_code}' "$base_url$path" "$@")
    if [[ $actual != "$expected" ]]; then
        echo "$method $path: expected HTTP $expected, got $actual" >&2
        # Creation can contain a token even if the response status is wrong.
        if [[ $method != POST || $path != /v1/agents ]]; then
            cat "$scratch/response.json" >&2
        fi
        return 1
    fi
}

check_process_user() {
    local uid
    uid=$(docker exec "$container" cat /proc/1/status | awk '$1 == "Uid:" { print $2 }')
    if [[ $uid != 10001 ]]; then
        echo "Expected server uid10001, got $uid" >&2
        return 1
    fi
}

stop_container() {
    local exit_code
    docker stop --time 10 "$container" >/dev/null
    exit_code=$(docker inspect "$container" --format '{{.State.ExitCode}}')
    if [[ $exit_code != 0 ]]; then
        echo "Server did not stop cleanly: exit $exit_code" >&2
        return 1
    fi
}

check_shipment() {
    request GET "/v1/packages/$id" 200 --header "Authorization: Bearer $token"
    jq --exit-status --arg id "$id" \
        '.id == $id and .title == "Container persistence check" and .merchant == "Test fixture" and
         .tracking.number == "9400111899223856926381" and .tracking.carrier == "USPS" and
         .sync.registration_state == "registered" and .sync.tracker_id == "trk_container_persistence" and .direction == "inbound"' \
        "$scratch/response.json" >/dev/null
}

check_pickup_journal() {
    request GET '/v1/pickups/resolve?reference=1Z999AA10123456784&carrier=ups' 200 --header "Authorization: Bearer $token"
    jq --exit-status '.state == "unknown" and .plan.tracking == ["1Z999AA10123456784"] and .plan.site.details.name == "Example site"' "$scratch/response.json" >/dev/null
    request GET '/v1/pickups/status' 200 --header "Authorization: Bearer $token"
    jq --exit-status '.uncertain == 1 and (.windows | length) == 0 and (has("address") | not)' "$scratch/response.json" >/dev/null
}

check_credentials() {
    local permissions
    permissions=$(docker exec "$container" stat -c '%a:%u:%g' /data/credentials.json)
    if [[ $permissions != 600:10001:10001 ]]; then
        echo "Expected credential mode/owner 600:10001:10001, got $permissions" >&2
        return 1
    fi
}

wait_for_api
jq --exit-status '.provider_enabled == true and .enabled_providers == ["doordash"]' \
    "$scratch/system.json" >/dev/null
check_process_user
request GET /v1/system 401
request POST /v1/packages 400 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: container-required-tracking-check' \
    --data '{"direction":"inbound","title":"Missing tracking"}'
request POST /v1/packages 400 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: container-blank-tracking-check' \
    --data '{"direction":"inbound","title":"Blank tracking","tracking":{"number":" "}}'
jq --exit-status '.error_code == "tracking_required"' "$scratch/response.json" >/dev/null
request GET /v1/packages 200 --header "Authorization: Bearer $token"
jq --exit-status '.items | length == 2' "$scratch/response.json" >/dev/null
id=$(jq --exit-status --raw-output '.items[] | select(.title == "Container persistence check") | .id' "$scratch/response.json")
agent_shipment_id=$(jq --exit-status --raw-output '.items[] | select(.title == "Agent shipment") | .id' "$scratch/response.json")
# Neither owner nor agent access may bypass provider verification. The imported
# fixture remains readable while missing provider configuration blocks writes.
request POST /v1/packages 503 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: container-persistence-check' \
    --data '{"direction":"inbound","title":"Unverified owner shipment","tracking":{"number":"9400111899223856926383","carrier":"USPS"}}'
jq --exit-status '.error_code == "tracking_provider_unconfigured"' "$scratch/response.json" >/dev/null
request GET /v1/packages 200 --header "Authorization: Bearer $token"
jq --exit-status '.items | length == 2' "$scratch/response.json" >/dev/null
check_shipment
check_pickup_journal

# Supervisor supplies a root-owned data directory and options file. Recreate
# that startup path with empty options and no configured owner credential.
stop_container
docker rm "$container" >/dev/null
docker run --rm --mount "type=volume,source=$volume,target=/data" \
    --entrypoint chown "$image" 0:0 /data
docker create --name "$container" \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$volume,target=/data" \
    "$image" >/dev/null
printf '{}\n' >"$scratch/options.json"
docker cp "$scratch/options.json" "$container:/data/options.json"
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"

for _ in {1..60}; do
    if docker cp "$container:/data/credentials.json" "$scratch/credentials.json" 2>/dev/null; then
        break
    fi
    sleep 0.25
done
if [[ ! -f $scratch/credentials.json ]]; then
    echo 'App did not generate private credentials' >&2
    exit 1
fi
check_credentials
jq --exit-status \
    '(.owner | length) >= 32 and (.integration | length) >= 32 and .owner != .integration' \
    "$scratch/credentials.json" >/dev/null
token=$(jq --exit-status --raw-output '.owner' "$scratch/credentials.json")
integration_token=$(jq --exit-status --raw-output '.integration' "$scratch/credentials.json")
wait_for_api
jq --exit-status '.credential_role == "owner"' "$scratch/system.json" >/dev/null
check_process_user
ownership=$(docker exec "$container" stat -c '%u:%g' /data /data/options.json)
if [[ $ownership != $'10001:10001\n10001:10001' ]]; then
    echo "App did not take ownership of Supervisor data/options: $ownership" >&2
    exit 1
fi
request GET /v1/system 401
request GET /v1/system 200 --header "Authorization: Bearer $integration_token"
jq --exit-status '.credential_role == "integration"' "$scratch/response.json" >/dev/null
request GET /v1/installation 403 --header "Authorization: Bearer $integration_token"
request POST /v1/installation/check 403 --header "Authorization: Bearer $integration_token"
request POST /v1/installation/check 200 --header "Authorization: Bearer $token"
jq --exit-status \
    '.backend.status == "pass" and .integration.status == "attention" and
     .authentication.status == "attention" and .easypost.status == "not_applicable" and
     .backup.status == "unavailable" and .last_backup == null' \
    "$scratch/response.json" >/dev/null
check_shipment
check_pickup_journal

request POST /v1/agents 201 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --data '{"name":"Container ingestion"}'
jq --exit-status \
    '.credential.name == "Container ingestion" and (.token | test("^shp_[0-9a-f]{64}$"))' \
    "$scratch/response.json" >/dev/null
agent_id=$(jq --exit-status --raw-output '.credential.id' "$scratch/response.json")
agent_token=$(jq --exit-status --raw-output '.token' "$scratch/response.json")
request GET /v1/agents 200 --header "Authorization: Bearer $token"
jq --exit-status --arg id "$agent_id" \
    '.items | length == 1 and .[0].id == $id and .[0].name == "Container ingestion" and
     (.[0] | keys == ["created_at", "id", "name"])' "$scratch/response.json" >/dev/null
request GET /v1/agents 403 --header "Authorization: Bearer $integration_token"
request GET /v1/system 200 --header "Authorization: Bearer $agent_token"
jq --exit-status '.credential_role == "agent"' "$scratch/response.json" >/dev/null
request POST /v1/packages 503 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: container-agent-check' \
    --data '{"direction":"inbound","title":"Saved by agent","tracking":{"number":"9400111899223856926382"}}'
jq --exit-status '.error_code == "tracking_provider_unconfigured"' "$scratch/response.json" >/dev/null
request GET /v1/packages 200 --header "Authorization: Bearer $agent_token"
jq --exit-status '.items | length == 2' "$scratch/response.json" >/dev/null
request GET "/v1/packages/$agent_shipment_id" 200 --header "Authorization: Bearer $agent_token"
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request PATCH "/v1/packages/$agent_shipment_id" 200 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' \
    --header "If-Match: \"$agent_revision\"" \
    --data '{"order_reference":"container-agent-order","tracking":{"number":"1Z999AA10123456784","carrier":"ups"}}'
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request POST "/v1/packages/$agent_shipment_id/sources" 200 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' \
    --header "If-Match: \"$agent_revision\"" \
    --data '{"namespace":"container-smoke","key":"agent-evidence"}'
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request POST "/v1/packages/$agent_shipment_id/refresh" 200 \
    --header "Authorization: Bearer $agent_token" \
    --header "If-Match: \"$agent_revision\""
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request DELETE "/v1/packages/$agent_shipment_id/status-override" 403 \
    --header "Authorization: Bearer $agent_token" \
    --header "If-Match: \"$agent_revision\""
request PUT "/v1/packages/$agent_shipment_id/archive" 403 \
    --header "Authorization: Bearer $agent_token" \
    --header "If-Match: \"$agent_revision\""
request GET /v1/installation 403 --header "Authorization: Bearer $agent_token"
request GET /v1/agents 403 --header "Authorization: Bearer $agent_token"
request POST /v1/agents 403 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' --data '{"name":"Forbidden agent"}'
request DELETE "/v1/agents/$agent_id" 403 --header "Authorization: Bearer $agent_token"

# Exercise the actual native SecureTar extraction/copy path. The observed host
# cannot restore modes/times after extraction assigns app ownership. Dropping
# FOWNER reproduces that boundary without granting the app any new capability.
case $expected_arch in
    amd64)
        supervisor_image='ghcr.io/home-assistant/amd64-hassio-supervisor@sha256:f5f3728cf7a9a637485576c7588d209a1527fe9d6bb46f7326ef9aa629c55713'
        ;;
    arm64)
        supervisor_image='ghcr.io/home-assistant/aarch64-hassio-supervisor@sha256:0f606324063cd5e7e591fb71efb5214ff56a05514898e33e461757950abc2914'
        ;;
    *)
        echo "Unsupported native restore architecture: $expected_arch" >&2
        exit 1
        ;;
esac
stop_container
# Deliberately fake and paused: this saved account can never contact a provider.
printf '%s\n' '{"refresh_token":"container-smoke-only","device_serial":"container-smoke-only","customer_id":"container-smoke-only","generation":1,"imports_enabled":false,"auth_required":true}' >"$scratch/amazon-account.json"
docker cp "$scratch/amazon-account.json" "$container:/data/amazon-account.json"
docker volume create "$outside_volume" >/dev/null
docker run --rm --mount "type=volume,source=$volume,target=/data" \
    --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        umask 077
        printf "outside-private-data\n" > /outside/sentinel
        mkdir /outside/private /data/owned-nested
        printf "preserved-private-state\n" > /data/owned-nested/marker
        chown -R 10001:10001 /data/owned-nested
        chown 10001:10001 /data/amazon-account.json
        ln -s /outside /data/external-directory
        ln -s /outside/sentinel /data/external-file
    '
docker rm "$container" >/dev/null
docker volume create "$restore_volume" >/dev/null
docker run --rm --network none --read-only --cap-drop FOWNER \
    --tmpfs /tmp:rw,nosuid,nodev,noexec \
    --mount "type=volume,source=$volume,target=/source,readonly" \
    --mount "type=volume,source=$restore_volume,target=/restored" \
    --entrypoint python3 --interactive "$supervisor_image" <<'PY'
import io
from pathlib import Path
import shutil
import tarfile
import tempfile

from securetar import SecureTarArchive, SecureTarFile, atomic_contents_add

# Synthetic application state only; this archive never leaves the helper.
password = "container-native-restore-only"
encrypted = io.BytesIO()
with SecureTarArchive(fileobj=encrypted, mode="w", password=password, create_version=3) as archive:
    with archive.create_tar("./app.tar.gz") as private:
        atomic_contents_add(private, Path("/source"), file_filter=lambda _: False, arcname="data")
encrypted.seek(0)
with tempfile.TemporaryDirectory() as extracted, tarfile.open(fileobj=encrypted, mode="r:") as outer:
    member = next(item for item in outer if item.name.endswith("app.tar.gz"))
    with SecureTarFile(fileobj=outer.extractfile(member), password=password, gzip=True) as private:
        private.extractall(path=extracted, filter="tar")
    # The mounted destination is already an empty directory; copy2 is the
    # native restore's default, including its loss of source ownership.
    shutil.copytree(Path(extracted, "data"), "/restored", symlinks=True, dirs_exist_ok=True)
for name in ("credentials.json", "amazon-account.json", "packages.sqlite3"):
    item = Path("/restored", name).stat()
    assert item.st_uid == 0 and item.st_gid == 0
    assert item.st_mode & 0o777 == 0o644, "Native extraction fixture did not reproduce permission loss"
PY
docker create --name "$container" \
    --cap-drop FOWNER \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$restore_volume,target=/data" \
    --mount "type=volume,source=$outside_volume,target=/outside" \
    "$image" >/dev/null
docker run --rm --mount "type=volume,source=$restore_volume,target=/data" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /data/credentials.json)" = 644:0:0
        test "$(stat -c "%a:%u:%g" /data/amazon-account.json)" = 644:0:0
        test -L /data/external-directory
        test -L /data/external-file
    '
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_credentials
ownership=$(docker exec "$container" stat -c '%a:%u:%g' \
    /data/packages.sqlite3 /data/amazon-account.json /data/owned-nested/marker)
if [[ $ownership != $'600:10001:10001\n600:10001:10001\n600:10001:10001' ]]; then
    echo 'Restored private data modes/owners were not repaired' >&2
    exit 1
fi
directories=$(docker exec "$container" stat -c '%a:%u:%g' /data /data/owned-nested)
if [[ $directories != $'700:10001:10001\n700:10001:10001' ]]; then
    echo 'Private directory modes/owners were not repaired' >&2
    exit 1
fi
outside=$(docker exec "$container" stat -c '%a:%u:%g' /outside/sentinel /outside/private)
if [[ $outside != $'600:0:0\n700:0:0' ]]; then
    echo 'Startup followed a private-data symlink outside the owned volume' >&2
    exit 1
fi
docker exec "$container" sh -c 'test "$(cat /outside/sentinel)" = outside-private-data'
docker cp "$container:/data/amazon-account.json" "$scratch/restored-amazon-account.json"
cmp -s "$scratch/amazon-account.json" "$scratch/restored-amazon-account.json"
docker cp "$container:/data/credentials.json" "$scratch/restarted-credentials.json"
if ! cmp -s "$scratch/credentials.json" "$scratch/restarted-credentials.json"; then
    echo 'App credentials changed after restart' >&2
    exit 1
fi
request GET /v1/system 200 --header "Authorization: Bearer $integration_token"
jq --exit-status '.credential_role == "integration"' "$scratch/response.json" >/dev/null
check_shipment
check_pickup_journal
request GET "/v1/packages/$agent_shipment_id" 200 --header "Authorization: Bearer $agent_token"
jq --exit-status --arg id "$agent_shipment_id" \
    '.id == $id and .title == "Agent shipment" and .order_reference == "container-agent-order" and
     .tracking.number == "1Z999AA10123456784" and .tracking.carrier == "UPS" and
     .sync.registration_state == "registered" and .sync.tracker_id == "trk_container_agent" and .direction == "outbound" and
     (.sources | any(.namespace == "container-smoke" and .key == "agent-evidence")) and
     .status_override == null and .archived == false' "$scratch/response.json" >/dev/null
request DELETE "/v1/agents/$agent_id" 204 --header "Authorization: Bearer $token"
request GET /v1/system 401 --header "Authorization: Bearer $agent_token"
stop_container
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_credentials
request GET /v1/system 401 --header "Authorization: Bearer $agent_token"
request GET /v1/agents 200 --header "Authorization: Bearer $token"
jq --exit-status '.items == []' "$scratch/response.json" >/dev/null
check_shipment
check_pickup_journal
stop_container

# Reject ambiguous ownership boundaries before starting the unprivileged server.
expect_startup_refusal() {
    local name=$1 running=true
    docker start "$name" >/dev/null
    for _ in {1..40}; do
        running=$(docker inspect "$name" --format '{{.State.Running}}')
        if [[ $running == false ]]; then
            break
        fi
        sleep 0.25
    done
    if [[ $running != false || $(docker inspect "$name" --format '{{.State.ExitCode}}') == 0 ]]; then
        echo 'App accepted a private-data ownership boundary violation' >&2
        return 1
    fi
}
docker create --name "$boundary_container" \
    --mount "type=volume,source=$restore_volume,target=/data" \
    --mount "type=volume,source=$outside_volume,target=/data/external-mount" \
    "$image" >/dev/null
expect_startup_refusal "$boundary_container"
docker create --name "$root_link_container" \
    --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c 'rmdir /data; ln -s /outside /data; exec /usr/local/bin/docker-entrypoint.sh' >/dev/null
expect_startup_refusal "$root_link_container"
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /outside/sentinel)" = 600:0:0
        test "$(stat -c "%a:%u:%g" /outside/private)" = 700:0:0
        test "$(stat -c "%u:%g" /outside)" = 0:0
    '

# A hard link can alias an outside inode without crossing a path or mount.
# Use a subpath of the fixture volume so the alias itself is inside /data.
docker rm "$boundary_container" "$root_link_container" >/dev/null
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint ln "$image" /outside/sentinel /outside/private/outside-alias
docker create --name "$boundary_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$outside_volume,target=/data,volume-subpath=private" \
    "$image" >/dev/null
expect_startup_refusal "$boundary_container"
failure=$(docker logs "$boundary_container" 2>&1)
if [[ $failure != *'outside hard links'* ]]; then
    echo 'Outside hard link was not rejected at the private-data preflight' >&2
    exit 1
fi
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /outside/sentinel)" = 600:0:0
        test "$(cat /outside/sentinel)" = outside-private-data
        rm /outside/private/outside-alias
        printf "internal-private-state\n" > /outside/private/internal-one
        chmod 600 /outside/private/internal-one
        ln /outside/private/internal-one /outside/private/internal-two
    '
# Atomic credential publication can leave two legitimate links after a crash.
# Both aliases are private, so startup must preserve and accept that state.
docker run --rm --name "$root_link_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$outside_volume,target=/data,volume-subpath=private" \
    "$image" --help >/dev/null
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g:%h" /outside/private/internal-one)" = 600:10001:10001:2
        test "$(stat -c "%a:%u:%g:%h" /outside/private/internal-two)" = 600:10001:10001:2
        test "$(cat /outside/private/internal-one)" = internal-private-state
        test "$(stat -c "%a:%u:%g" /outside/sentinel)" = 600:0:0
    '

echo "Container smoke passed ($expected_arch): uid10001, standalone/app/agent access, native private-state restore, unchanged credentials, repaired modes, safe restart, external symlink/mount/hardlink boundaries, internal hardlinks, installation checks, and durable packages/credentials/revocations."

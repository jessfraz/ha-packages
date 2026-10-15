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
# Deliberately fake: this token exists only in the disposable local container.
token='container-smoke-only-no-provider-credentials'

cleanup() {
    status=$?
    if [[ $status -ne 0 ]]; then
        docker logs "$container" >&2 || true
    fi
    docker rm --force "$container" >/dev/null 2>&1 || true
    docker volume rm "$volume" >/dev/null 2>&1 || true
    rm -rf "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker volume create "$volume" >/dev/null
docker run --detach --name "$container" \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$volume,target=/data" \
    --env "SHIPMENTS_API_TOKEN=$token" \
    "$image" >/dev/null
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
    request GET "/v1/shipments/$id" 200 --header "Authorization: Bearer $token"
    jq --exit-status --arg id "$id" \
        '.id == $id and .title == "Container persistence check" and .merchant == "Test fixture"' \
        "$scratch/response.json" >/dev/null
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
check_process_user
request GET /v1/system 401
request POST /v1/shipments 201 \
    --header "Authorization: Bearer $token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: container-persistence-check' \
    --data '{"title":"Container persistence check","merchant":"Test fixture"}'
id=$(jq --exit-status --raw-output '.id' "$scratch/response.json")

stop_container
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_shipment

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
request POST /v1/shipments 201 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' \
    --header 'Idempotency-Key: container-agent-check' \
    --data '{"title":"Agent shipment"}'
agent_shipment_id=$(jq --exit-status --raw-output '.id' "$scratch/response.json")
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request PATCH "/v1/shipments/$agent_shipment_id" 200 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' \
    --header "If-Match: \"$agent_revision\"" \
    --data '{"order_reference":"container-agent-order"}'
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request POST "/v1/shipments/$agent_shipment_id/sources" 200 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' \
    --header "If-Match: \"$agent_revision\"" \
    --data '{"namespace":"container-smoke","key":"agent-evidence"}'
agent_revision=$(jq --exit-status --raw-output '.revision' "$scratch/response.json")
request DELETE "/v1/shipments/$agent_shipment_id/status-override" 403 \
    --header "Authorization: Bearer $agent_token" \
    --header "If-Match: \"$agent_revision\""
request PUT "/v1/shipments/$agent_shipment_id/archive" 403 \
    --header "Authorization: Bearer $agent_token" \
    --header "If-Match: \"$agent_revision\""
request GET /v1/installation 403 --header "Authorization: Bearer $agent_token"
request GET /v1/agents 403 --header "Authorization: Bearer $agent_token"
request POST /v1/agents 403 \
    --header "Authorization: Bearer $agent_token" \
    --header 'Content-Type: application/json' --data '{"name":"Forbidden agent"}'
request DELETE "/v1/agents/$agent_id" 403 --header "Authorization: Bearer $agent_token"

stop_container
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
check_credentials
docker cp "$container:/data/credentials.json" "$scratch/restarted-credentials.json"
if ! cmp -s "$scratch/credentials.json" "$scratch/restarted-credentials.json"; then
    echo 'App credentials changed after restart' >&2
    exit 1
fi
request GET /v1/system 200 --header "Authorization: Bearer $integration_token"
jq --exit-status '.credential_role == "integration"' "$scratch/response.json" >/dev/null
check_shipment
request GET "/v1/shipments/$agent_shipment_id" 200 --header "Authorization: Bearer $agent_token"
jq --exit-status --arg id "$agent_shipment_id" \
    '.id == $id and .title == "Agent shipment" and .order_reference == "container-agent-order" and
     (.sources | any(.namespace == "container-smoke" and .key == "agent-evidence")) and
     .status_override == null and .archived == false' "$scratch/response.json" >/dev/null
request DELETE "/v1/agents/$agent_id" 204 --header "Authorization: Bearer $token"
request GET /v1/system 401 --header "Authorization: Bearer $agent_token"
stop_container
docker start "$container" >/dev/null
base_url="http://$(docker port "$container" 8080/tcp)"
wait_for_api
request GET /v1/system 401 --header "Authorization: Bearer $agent_token"
request GET /v1/agents 200 --header "Authorization: Bearer $token"
jq --exit-status '.items == []' "$scratch/response.json" >/dev/null
check_shipment
stop_container

echo "Container smoke passed ($expected_arch): uid10001, standalone/app/agent access, private credentials, installation checks, and durable shipments/credentials/revocations after clean restart."

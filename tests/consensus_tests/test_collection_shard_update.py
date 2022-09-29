import pathlib

from .utils import *

N_PEERS = 3
N_SHARDS = 5


def test_collection_shard_update(tmp_path: pathlib.Path):
    assert_project_root()
    peer_dirs = make_peer_folders(tmp_path, N_PEERS)

    # Gathers REST API uris
    peer_api_uris = []

    # Start bootstrap
    (bootstrap_api_uri, bootstrap_uri) = start_first_peer(
        peer_dirs[0], "peer_0_0.log")
    peer_api_uris.append(bootstrap_api_uri)

    # Wait for leader
    leader = wait_peer_added(bootstrap_api_uri)

    # Start other peers
    for i in range(1, len(peer_dirs)):
        peer_api_uris.append(start_peer(
            peer_dirs[i], f"peer_0_{i}.log", bootstrap_uri))

    # Wait for cluster
    wait_for_uniform_cluster_status(peer_api_uris, leader)

    # Check that there are no collections on all peers
    for uri in peer_api_uris:
        r = requests.get(f"{uri}/collections")
        assert_http_ok(r)
        assert len(r.json()["result"]["collections"]) == 0

    # Create collection in first peer
    r = requests.put(
        f"{peer_api_uris[0]}/collections/test_collection", json={
            "vectors": {
                "image": {
                    "size": 4,
                    "distance": "Dot"
                },
                "text": {
                    "size": 4,
                    "distance": "Cosine"
                }
            },
            "shard_number": N_SHARDS,
        })
    assert_http_ok(r)

    # Check that it exists on all peers
    wait_for_uniform_collection_existence("test_collection", peer_api_uris)

    # Check collection's cluster info
    collection_cluster_info = get_collection_cluster_info(peer_api_uris[0], "test_collection")
    assert collection_cluster_info["shard_count"] == N_SHARDS

    # Create malformed points in first peer's collection
    r = requests.put(
        f"{peer_api_uris[0]}/collections/test_collection/points?wait=true", json={
            "points": [
                {
                    "id": 1,
                    "vector": {
                        "image": [0.05, 0.61, 0.76, 0.74],
                        "text": [0.05, 0.61, 0.76, 0.74]
                    }
                },
                {
                    "id": 2,
                    "vector": {
                        "image": [0.05, 0.61, 0.76, 0.74]
                    }
                },
                {
                    "id": 3,
                    "vector": {
                        "image": [0.05, 0.61, 0.76, 0.74],
                        "text": [0.05, 0.61, 0.76, 0.74]
                    }
                },
                {
                    "id": 4,
                    "vector": {
                        "image": [0.05, 0.61, 0.76, 0.74],
                        "text": [0.05, 0.61, 0.76, 0.74]
                    }
                }
            ]
        })
 
    assert r.status_code == 400
    error = r.json()["status"]["error"]
    assert error.__contains__("Wrong input: 1 out of 3 shards failed to apply operation")
    assert error.__contains__("Wrong input: Missed vector name error: text")


# python-client/submit_example.py
import json, hashlib, time, os
import requests
from nacl.signing import SigningKey
from nacl.encoding import HexEncoder

SERVER = "http://localhost:8080"

# Miner keys: load miner private key from secure file (hex)
sk_hex = open("python-client/miner1_sk.hex").read().strip()
sk = SigningKey(sk_hex.encode(), encoder=HexEncoder)
pk = sk.verify_key
print("public key:", pk.encode(encoder=HexEncoder).decode())

def build_repro_package(hyperparams: dict, out_path: str):
    # create zip containing hyperparameters.json + train.py
    import zipfile
    tmpdir = "pkgtmp"
    os.makedirs(tmpdir, exist_ok=True)
    with open(os.path.join(tmpdir, "hyperparameters.json"), "w") as fh:
        json.dump(hyperparams, fh)
    # assume train.py exists in same repo
    os.system(f"cp python-client/train.py {tmpdir}/train.py")
    zipf = zipfile.ZipFile(out_path, "w")
    for name in os.listdir(tmpdir):
        zipf.write(os.path.join(tmpdir, name), arcname=name)
    zipf.close()

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        while True:
            chunk = fh.read(8192)
            if not chunk: break
            h.update(chunk)
    return h.hexdigest()

def sign_payload(sk, payload: dict):
    msg = json.dumps(payload, sort_keys=True, separators=(',', ':')).encode()
    sig = sk.sign(msg).signature
    return sig.hex()

def submit_example():
    hyper = {"layers":[128,64], "activation":"relu", "lr":0.001}
    artifact = "repro.zip"
    build_repro_package(hyper, artifact)
    art_hash = sha256_file(artifact)

    payload = {
        "task_id": "task-prod-001",
        "miner_id": 1,
        "performance": 0.92,
        "artifact_hash": art_hash,
        "hyperparameters": hyper,
        "timestamp": int(time.time()),
        "nonce": int.from_bytes(os.urandom(8), "big")
    }
    signature = sign_payload(sk, payload)

    files = {
        "payload": (None, json.dumps(payload)),
        "signature": (None, signature),
        "artifact": ("repro.zip", open(artifact, "rb"), "application/zip")
    }
    r = requests.post(SERVER + "/submit", files=files)
    print(r.status_code, r.text)

if __name__ == "__main__":
    submit_example()

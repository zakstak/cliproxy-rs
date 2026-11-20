"""Build independently specified S3 SigV4 vectors with Python's standard library.

Authority: AWS Signature Version 4, canonical request and signing-key steps:
https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html

Canonical requests below are explicit protocol examples. This script does not import
or execute cliproxy-rs or any Go library. Run from any directory to regenerate the
adjacent JSON file. Access and secret keys are public AWS documentation placeholders.
"""
import hashlib
import hmac
import json
from pathlib import Path

EMPTY = hashlib.sha256(b"").hexdigest()
DATE = "20261114T120000Z"
ACCESS = "AKIAIOSFODNN7EXAMPLE"
SECRET = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"

# method, decoded path, ordered query pairs, additional headers, host, region,
# independently written canonical URI, canonical query, canonical header names.
CASES = [
    ("HEAD", "/bucket", [], [], "s3.example.invalid", "us-east-1", "/bucket", "", "host;x-amz-date"),
    ("GET", "/bucket", [["location", ""]], [], "s3.example.invalid", "us-east-1", "/bucket", "location=", "host;x-amz-date"),
    ("GET", "/bucket", [["prefix", "auths/"], ["list-type", "2"], ["continuation-token", "a b/c="]], [], "s3.example.invalid", "eu-west-1", "/bucket", "continuation-token=a%20b%2Fc%3D&list-type=2&prefix=auths%2F", "host;x-amz-date"),
    ("PUT", "/bucket/team a/ü+x.json", [], [["Content-Type", "application/json"]], "s3.example.invalid", "us-east-1", "/bucket/team%20a/%C3%BC%2Bx.json", "", "content-type;host;x-amz-date"),
    ("DELETE", "/bucket/auths/a.json", [], [], "127.0.0.1:9000", "us-east-1", "/bucket/auths/a.json", "", "host;x-amz-date"),
    ("PUT", "/new-bucket", [], [["X-Amz-Content-Sha256", EMPTY]], "s3.example.invalid", "ap-south-1", "/new-bucket", "", "host;x-amz-content-sha256;x-amz-date"),
]

def mac(key, message):
    return hmac.new(key, message.encode(), hashlib.sha256).digest()

vectors = []
for method, path, query, headers, host, region, uri, canonical_query, signed in CASES:
    values = {"host": host, "x-amz-date": DATE}
    values.update({key.lower(): value for key, value in headers})
    canonical_headers = "".join(f"{key}:{values[key]}\n" for key in signed.split(";"))
    canonical = "\n".join([method, uri, canonical_query, canonical_headers, signed, EMPTY])
    scope = f"{DATE[:8]}/{region}/s3/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", DATE, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = mac(("AWS4" + SECRET).encode(), DATE[:8])
    for part in [region, "s3", "aws4_request"]:
        key = mac(key, part)
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    vectors.append(dict(method=method, path=path, query=query, headers=headers, host=host, region=region,
                        payload_sha256=EMPTY, canonical_request=canonical,
                        authorization=f"AWS4-HMAC-SHA256 Credential={ACCESS}/{scope}, SignedHeaders={signed}, Signature={signature}"))
output = dict(source="AWS Signature Version 4 specification; explicit canonical requests signed by Python hashlib/hmac, not proxy output", date=DATE, access_key=ACCESS, secret_key=SECRET, cases=vectors)
Path(__file__).with_name("sigv4_contract.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")

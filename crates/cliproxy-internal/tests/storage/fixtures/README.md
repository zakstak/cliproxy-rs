# AWS signing cases

`sigv4_contract.json` contains explicit canonical URI, query, and header cases for
the [AWS Signature Version 4 signing contract](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html).
`build_sigv4_contract.py` calculates the expected signatures with Python's standard
`hashlib` and `hmac` modules. It does not import or execute proxy code or Go code.
The keys are AWS's public example placeholders.

The Rust tests compare signatures with these independent calculations and verify
that header case, header order, and irrelevant headers do not change signatures.
The former MinIO Go output fixture and writer were removed in issue #198. Git
history retains their provenance.

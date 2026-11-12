// Records Go's xAI credential file names (CLIProxyAPI internal/auth/xai/filename.go at
// v8.0.67 8bba7a77, introduced by d65840fb). The three functions below are copied
// verbatim from that file; the rest is this generator. Standard library only; it
// opens no sockets and writes only the fixture path it is given.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"runtime"
	"strings"
	"time"
)

var cases = [][2]string{
	// email, subject
	{"a@b.com", "s"},
	{"  A.B+c@x.ai ", ""},
	{"A.B+C@X.AI", ""},
	{"a.b+c@x.ai", "other-subject"},
	{"", "user|1"},
	{"", "user-1"},
	{"é@x.ai", ""},
	{"---", "sub"},
	{"-a b-", ""},
	{"", " -z- "},
	// Distinct accounts whose sanitized names collide.
	{"a+b@x.ai", ""},
	{"a/b@x.ai", ""},
	{"a b@x.ai", ""},
	{"", "acct/7"},
	{"", "acct 7"},
	// Nothing survives sanitization.
	{"+++@", ""},
	{"", "!!!"},
	// Case is folded for emails only; subjects are case-sensitive identifiers.
	{"", "Sub-A"},
	{"", "sub-a"},
	{strings.Repeat("x", 200) + "@x.ai", ""},
}

type entry struct {
	Email   string `json:"email"`
	Subject string `json:"subject"`
	Name    string `json:"name"`
	Legacy  string `json:"legacy"`
}

func main() {
	var out struct {
		Go    string  `json:"go"`
		Names []entry `json:"names"`
	}
	out.Go = runtime.Version()
	for _, c := range cases {
		out.Names = append(out.Names, entry{c[0], c[1], CredentialFileName(c[0], c[1]), legacyCredentialFileName(c[0], c[1])})
	}
	raw, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(raw, '\n'), 0o644); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

// CredentialFileName returns the filename used to persist xAI OAuth credentials.
// It keeps a readable account name and appends a hash of the original identity
// so distinct emails that sanitize identically cannot overwrite each other.
func CredentialFileName(email, subject string) string {
	cleanEmail := strings.TrimSpace(email)
	if cleanEmail != "" {
		// Emails are case-insensitive, so normalize case to keep the same
		// account on the same file regardless of the case returned upstream.
		// The hash covers the normalized email only so the filename stays
		// stable even when a later login stops returning a subject.
		normalizedEmail := strings.ToLower(cleanEmail)
		sanitized := sanitizeFileSegment(normalizedEmail)
		if sanitized == "" {
			sanitized = "account"
		}
		// Leave room for the prefix, identity hash and extension on common filesystems.
		if len(sanitized) > 120 {
			sanitized = sanitized[:120]
		}
		digest := sha256.Sum256([]byte("email\x00" + normalizedEmail))
		return fmt.Sprintf("xai-%s-%s.json", sanitized, hex.EncodeToString(digest[:8]))
	}
	cleanSubject := strings.TrimSpace(subject)
	if cleanSubject != "" {
		sanitized := sanitizeFileSegment(cleanSubject)
		if sanitized == "" {
			sanitized = "account"
		}
		if len(sanitized) > 120 {
			sanitized = sanitized[:120]
		}
		digest := sha256.Sum256([]byte("subject\x00" + cleanSubject))
		return fmt.Sprintf("xai-%s-%s.json", sanitized, hex.EncodeToString(digest[:8]))
	}
	return fmt.Sprintf("xai-%d.json", time.Now().UnixMilli())
}

func sanitizeFileSegment(value string) string {
	value = strings.TrimSpace(value)
	if value == "" {
		return ""
	}
	var b strings.Builder
	for _, r := range value {
		switch {
		case r >= 'a' && r <= 'z':
			b.WriteRune(r)
		case r >= 'A' && r <= 'Z':
			b.WriteRune(r)
		case r >= '0' && r <= '9':
			b.WriteRune(r)
		case r == '@' || r == '.' || r == '_' || r == '-':
			b.WriteRune(r)
		default:
			b.WriteRune('-')
		}
	}
	return strings.Trim(b.String(), "-")
}

// legacyCredentialFileName reconstructs the pre-hash filename format so a
// credential saved before the hash suffix was introduced can be migrated.
func legacyCredentialFileName(email, subject string) string {
	if sanitized := sanitizeFileSegment(email); sanitized != "" {
		return fmt.Sprintf("xai-%s.json", sanitized)
	}
	if sanitized := sanitizeFileSegment(subject); sanitized != "" {
		return fmt.Sprintf("xai-%s.json", sanitized)
	}
	return ""
}

//! A deliberately unindexed reference model: scan complete trajectories and keep an
//! ordered vector for eviction. It knows no production prefix/tail indexes. Generated
//! histories are at most three depth-tagged turns, so they cannot create a shifted
//! two-turn compaction overlap; compaction has separate adversarial scenario tests.
use super::*;

#[derive(Clone)]
struct Entry {
    namespace: String,
    sequence: Vec<String>,
    tails: Vec<String>,
    minimum: usize,
    auth: String,
    lineage: Match,
    expires: i64,
    generation: u64,
    recent: u64,
}

struct Reference {
    entries: Vec<Entry>,
    limits: Limits,
    generation: u64,
    recent: u64,
    operations: u64,
}

// The identity scheme is an on-disk/operator-visible contract. Build its byte
// framing directly, without calling the matcher's rolling-key or ID helpers.
fn identity(namespace: &str, sequence: &[String]) -> String {
    let mut digest = vec![0; 32];
    for value in sequence {
        let mut bytes = digest;
        bytes.push(0);
        bytes.extend_from_slice(value.as_bytes());
        digest = Sha256::digest(bytes).to_vec();
    }
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let key = format!("{}:{hex}", sequence.len());
    let seed = [
        b"cli-proxy-api:lcp-session:v1\0".as_slice(),
        namespace.as_bytes(),
        b"\0",
        key.as_bytes(),
    ]
    .concat();
    format!("lcp:v1:{:x}", Sha256::digest(seed))
}

impl Reference {
    fn new(limits: Limits) -> Self {
        Self {
            entries: Vec::new(),
            limits,
            generation: 0,
            recent: 0,
            operations: 0,
        }
    }

    fn tick(&mut self, now: i64) {
        self.operations += 1;
        if self.operations.is_multiple_of(128) {
            self.entries.retain(|entry| entry.expires > now);
        }
    }

    fn access(&mut self, index: usize, now: i64, refresh: bool) -> u64 {
        self.generation += 1;
        self.recent += 1;
        let entry = &mut self.entries[index];
        entry.generation = self.generation;
        entry.recent = self.recent;
        if refresh {
            entry.expires = now + self.limits.ttl.as_secs() as i64;
        }
        self.generation
    }

    fn find(&mut self, namespace: &str, p: &Prepared, now: i64, refresh: bool) -> Option<Match> {
        let candidate = &p.fingerprints[..p.fingerprints.len().min(self.limits.max_turns)];
        // Rank complete trajectories, rather than searching a hash index.
        let best = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if entry.namespace != namespace || entry.expires <= now {
                    return None;
                }
                let common = entry.sequence.iter().zip(candidate).take_while(|(a, b)| a == b).count();
                (common >= p.min_prefix.max(entry.minimum)).then_some((
                    common,
                    entry.sequence.len(),
                    entry.generation,
                    entry.expires,
                    index,
                ))
            })
            .max()?;
        let common = best.0;
        let index = best.4;
        let entry = &self.entries[index];
        let mut found = entry.lineage.clone();
        found.auth = entry.auth.clone();
        found.prefix_length = common;
        found.fork = false;
        if common < entry.sequence.len() && common < candidate.len() {
            found.session = identity(namespace, &candidate[..common + 1]);
            found.parent = identity(namespace, &candidate[..common]);
            found.fork = true;
            found.compaction = false;
            found.node_kind = "fork".into();
        }
        found.access = self.access(index, now, refresh);
        Some(found)
    }

    fn bind(&mut self, namespace: &str, p: &Prepared, auth: &str, now: i64) -> Match {
        let sequence = p.fingerprints[..p.fingerprints.len().min(self.limits.max_turns)].to_vec();
        let exact = self
            .entries
            .iter()
            .position(|e| e.namespace == namespace && e.sequence == sequence);
        let live = exact.and_then(|index| {
            let old = self.entries.remove(index);
            (old.expires > now).then_some(old.lineage)
        });
        let mut lineage = live
            .or_else(|| self.find(namespace, p, now, true))
            .unwrap_or_else(|| Match {
                session: identity(namespace, &sequence[..p.min_prefix]),
                ..Match::default()
            });
        lineage.auth.clear();
        lineage.prefix_length = 0;
        // A successful bind reserves and then publishes its removal generation.
        self.generation += 2;
        self.recent += 1;
        lineage.access = self.generation;
        self.entries.push(Entry {
            namespace: namespace.into(),
            sequence,
            tails: p.fingerprints.clone(),
            minimum: p.min_prefix,
            auth: auth.into(),
            lineage: lineage.clone(),
            expires: now + self.limits.ttl.as_secs() as i64,
            generation: self.generation,
            recent: self.recent,
        });
        while self.entries.len() > self.limits.max_groups
            || self.entries.iter().map(|e| e.sequence.len()).sum::<usize>() > self.limits.max_prefixes
        {
            let index = self.entries.iter().enumerate().min_by_key(|(_, e)| e.recent).unwrap().0;
            self.entries.remove(index);
        }
        lineage
    }

    fn touch(&mut self, namespace: &str, p: &Prepared, auth: &str, now: i64) -> bool {
        let sequence = &p.fingerprints[..p.fingerprints.len().min(self.limits.max_turns)];
        if let Some(index) = self
            .entries
            .iter()
            .position(|e| e.namespace == namespace && e.sequence == sequence && e.expires > now)
        {
            if self.entries[index].auth != auth && self.entries[index].auth != "home-pending" {
                return false;
            }
            self.entries[index].auth = auth.into();
            self.access(index, now, true);
            // Updating an uncapped trailing window publishes a fresh index generation.
            if self.entries[index].tails != p.fingerprints {
                self.entries[index].tails = p.fingerprints.clone();
                self.access(index, now, false);
            }
        } else {
            self.bind(namespace, p, auth, now);
        }
        true
    }

    fn remove(&mut self, namespace: &str, p: &Prepared, auth: &str, generation: u64) -> bool {
        let sequence = &p.fingerprints[..p.fingerprints.len().min(self.limits.max_turns)];
        let Some(index) = self.entries.iter().position(|e| {
            e.namespace == namespace
                && e.sequence == sequence
                && e.auth == auth
                && (generation == 0 || e.generation <= generation)
        }) else {
            return false;
        };
        self.entries.remove(index);
        true
    }

    fn lookup(&mut self, session: &str, now: i64) -> Option<(Vec<String>, String)> {
        self.entries.retain(|e| e.lineage.session != session || e.expires > now);
        let mut matching = self.entries.iter().filter(|e| e.lineage.session == session);
        let first = matching.next()?;
        let mut auths = vec![first.auth.clone()];
        auths.extend(matching.map(|e| e.auth.clone()));
        auths.sort();
        auths.dedup();
        Some((auths, first.namespace.clone()))
    }
}

#[test]
fn generated_matcher_histories_agree_with_unindexed_reference_model() {
    let base = Instant::now() + Duration::from_secs(10_000);
    for seed in 1..=12u64 {
        let limits = Limits {
            ttl: Duration::from_secs(17),
            max_turns: 2 + seed as usize % 2,
            max_groups: 3 + seed as usize % 8,
            max_prefixes: 7 + seed as usize % 9,
        };
        let mut matcher = Matcher::new(limits);
        let mut reference = Reference::new(limits);
        let mut random = seed;
        let mut clock = 0i64;
        let mut sessions = Vec::new();
        for step in 0..1500 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let namespace = ["caller-a/model-a", "caller-b/model-a", "caller-a/model-b"][(random as usize >> 8) % 3];
            let auth = ["auth-a", "auth-b", "home-pending"][(random as usize >> 16) % 3];
            let length = 1 + (random as usize >> 24) % 3;
            let sequence = [
                format!("root-{}", (random >> 32) % 3),
                format!("middle-{}", (random >> 36) % 3),
                format!("leaf-{}", (random >> 40) % 3),
            ];
            let p = Prepared {
                fingerprints: sequence[..length].to_vec(),
                min_prefix: 1,
                tails: Vec::new(),
                environment: sequence[0].clone(),
            };
            // Exercise expiration equality and clocks that briefly move backwards.
            clock += match random % 11 {
                0 => -3,
                1 => 17,
                _ => 1,
            };
            let now = if clock >= 0 {
                base + Duration::from_secs(clock as u64)
            } else {
                base - Duration::from_secs(clock.unsigned_abs())
            };
            let operation = (random >> 48) % 13;
            let context = format!("seed={seed} step={step} op={operation} t={clock} {namespace} {p:?}");
            match operation {
                0..=2 => {
                    reference.tick(clock);
                    let want = reference.bind(namespace, &p, auth, clock);
                    let got = matcher.bind(namespace, &p, auth, now).unwrap();
                    assert_eq!(got, want, "{context}");
                    sessions.push(got.session);
                }
                3..=5 => {
                    reference.tick(clock);
                    let refresh = operation != 5;
                    let want = reference.find(namespace, &p, clock, refresh);
                    let got = if refresh {
                        matcher.find(namespace, &p, now)
                    } else {
                        matcher.peek(namespace, &p, now)
                    };
                    assert_eq!(got, want, "{context}");
                }
                6 => {
                    reference.tick(clock);
                    assert_eq!(
                        matcher.touch(namespace, &p, auth, now),
                        reference.touch(namespace, &p, auth, clock),
                        "{context}"
                    );
                }
                7 => {
                    let generation = if random & 1 == 0 {
                        0
                    } else {
                        reference.generation.saturating_sub(3)
                    };
                    reference.tick(clock);
                    assert_eq!(
                        matcher.remove(namespace, &p.fingerprints, auth, generation, now),
                        reference.remove(namespace, &p, auth, generation),
                        "{context}"
                    );
                }
                8 => {
                    matcher.invalidate(auth);
                    reference.entries.retain(|e| e.auth != auth);
                }
                9 => {
                    let session = sessions
                        .get((random as usize >> 12) % sessions.len().max(1))
                        .map(String::as_str)
                        .unwrap_or("missing");
                    assert_eq!(
                        matcher.lookup(session, now),
                        reference.lookup(session, clock),
                        "{context}"
                    );
                }
                10 => {
                    matcher.retain(|a| a != auth);
                    reference.entries.retain(|e| e.auth != auth);
                }
                11 if step % 7 == 0 => {
                    matcher.clear();
                    reference.entries.clear();
                }
                12 if step % 11 == 0 => {
                    matcher.reset(limits);
                    reference.entries.clear();
                    reference.operations = 0;
                    reference.recent = 0;
                }
                _ => {}
            }
        }
    }
}

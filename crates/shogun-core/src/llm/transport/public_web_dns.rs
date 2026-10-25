//! DNS checks at reqwest's connection seam, without a second unchecked lookup.

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

pub(super) struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str().to_owned(), 0)).await?;
            Ok(Box::new(addresses) as Addrs)
        })
    }
}

/// The resolver's refusal, typed so the transport can tell it apart from a network failure.
#[derive(Debug)]
struct NotPublic;

impl std::fmt::Display for NotPublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("web DNS destination is not public")
    }
}

impl std::error::Error for NotPublic {}

/// Whether `error` (anywhere in its source chain) is this guard's refusal.
pub(super) fn refused(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(error) = source {
        if error.is::<NotPublic>() {
            return true;
        }
        source = error.source();
    }
    false
}

pub(super) struct PublicWebResolver<R>(pub R);

impl<R: Resolve> Resolve for PublicWebResolver<R> {
    fn resolve(&self, name: Name) -> Resolving {
        let resolving = self.0.resolve(name);
        Box::pin(async move {
            // Only vetted addresses reach the connector, which connects directly to them; the
            // URL hostname remains intact for Host, SNI and certificates. Non-public answers are
            // dropped rather than failing the whole read, so one stale or DNS64-synthesized
            // record cannot make a site with a usable public address unreadable.
            let addresses: Vec<_> = resolving
                .await?
                .filter(|address| crate::web_fetch::is_public_ip(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(Box::new(NotPublic) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct FixtureResolver {
        answers: Vec<Vec<SocketAddr>>,
        calls: Arc<AtomicUsize>,
    }

    impl Resolve for FixtureResolver {
        fn resolve(&self, _: Name) -> Resolving {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let addresses = self.answers[index.min(self.answers.len() - 1)].clone();
            Box::pin(async move { Ok(Box::new(addresses.into_iter()) as Addrs) })
        }
    }

    fn fixture(answers: &[&[&str]]) -> FixtureResolver {
        FixtureResolver {
            answers: answers
                .iter()
                .map(|answer| {
                    answer
                        .iter()
                        .map(|address| address.parse().expect("fixture address"))
                        .collect()
                })
                .collect(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[tokio::test]
    async fn public_hostname_cannot_resolve_to_non_public_addresses() {
        for address in [
            "127.0.0.1:0",
            "10.0.0.1:0",
            "169.254.169.254:0",
            "100.64.0.1:0",
            "192.0.2.1:0",
            "198.18.0.1:0",
            "224.0.0.1:0",
            "240.0.0.1:0",
            "[::1]:0",
            "[fd00::1]:0",
            "[fe80::1]:0",
            "[ff02::1]:0",
            "[::ffff:10.0.0.1]:0",
            "[64:ff9b::a00:1]:0",
            "[2002:a00:1::]:0",
            "[2001:db8::1]:0",
            "[3fff::1]:0",
        ] {
            let resolver = PublicWebResolver(fixture(&[&[address]]));
            assert!(
                resolver
                    .resolve("public.example".parse().expect("name"))
                    .await
                    .is_err(),
                "{address}"
            );
        }
    }

    #[tokio::test]
    async fn empty_and_all_private_answers_fail_before_connecting() {
        for answers in [&[][..], &["10.0.0.1:0", "[fd00::1]:0"][..]] {
            let resolver = PublicWebResolver(fixture(&[answers]));
            assert!(resolver
                .resolve("public.example".parse().expect("name"))
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn mixed_answers_hand_only_public_addresses_to_the_connector() {
        for answers in [&["8.8.8.8:0", "10.0.0.1:0"][..], &["10.0.0.1:0", "8.8.8.8:0"][..]] {
            let resolver = PublicWebResolver(fixture(&[answers]));
            let addresses: Vec<_> = resolver
                .resolve("public.example".parse().expect("name"))
                .await
                .expect("public address remains")
                .collect();
            assert_eq!(addresses, vec!["8.8.8.8:0".parse::<SocketAddr>().expect("address")]);
        }
    }

    #[tokio::test]
    async fn dns64_answers_are_judged_by_their_embedded_ipv4() {
        // 64:ff9b::/96 (RFC 6052) is how an IPv6-only network reaches IPv4-only sites.
        let resolver = PublicWebResolver(fixture(&[&["[64:ff9b::808:808]:0"]]));
        assert!(resolver.resolve("public.example".parse().expect("name")).await.is_ok());
        let resolver = PublicWebResolver(fixture(&[&["[64:ff9b::7f00:1]:0"]]));
        assert!(resolver.resolve("public.example".parse().expect("name")).await.is_err());
    }

    #[tokio::test]
    async fn connector_receives_exact_checked_answer_without_another_lookup() {
        let raw = fixture(&[&["8.8.8.8:0", "[2001:4860:4860::8888]:0"], &["10.0.0.1:0"]]);
        let calls = raw.calls.clone();
        let resolver = PublicWebResolver(raw);
        let addresses: Vec<_> = resolver
            .resolve("public.example".parse().expect("name"))
            .await
            .expect("public result")
            .collect();
        assert_eq!(
            addresses,
            vec![
                "8.8.8.8:0".parse().expect("address"),
                "[2001:4860:4860::8888]:0".parse().expect("address")
            ]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A later connection gets its own fresh validation, so rebinding cannot supply LAN IPs.
        assert!(resolver
            .resolve("public.example".parse().expect("name"))
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reqwest_connection_uses_guard() {
        let raw = fixture(&[&["10.0.0.1:0"]]);
        let calls = raw.calls.clone();
        let client = super::super::public_web_client_builder(raw)
            .build()
            .expect("client");
        let request = client
            .get("https://public.example/path")
            .build()
            .expect("request");
        // Only synthetic DNS: the guard fails before any socket is attempted.
        let error = client
            .execute(request)
            .await
            .expect_err("private DNS must fail");
        assert!(error.is_connect());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(refused(&error), "connection must fail at destination policy");
    }

    #[tokio::test]
    async fn hostname_passing_the_url_policy_still_requires_public_dns_answer() {
        let target = "https://redirect.example/next";
        // A hostname that passes the URL policy (as a redirect hop would) is still rejected by
        // its resolved address. Every hop goes through this same connector.
        assert!(super::super::redirect_decision(target, 1).is_ok());
        let raw = fixture(&[&["169.254.169.254:0"]]);
        let calls = raw.calls.clone();
        let client = super::super::public_web_client_builder(raw)
            .build()
            .expect("client");
        assert!(client
            .get(target)
            .send()
            .await
            .expect_err("redirect DNS")
            .is_connect());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

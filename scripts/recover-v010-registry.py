#!/usr/bin/env python3
"""Authenticated, fail-closed GHCR checks for the fixed v0.1.0 recovery only."""
import argparse
import base64
import hashlib
import json
import os
import re
import urllib.error
import urllib.parse
import urllib.request

SOURCE = 'f4b7c3794125ae8169ff4ab65c2f378267f5ca5c'
REPOSITORY = 'torutesu/shogunagent'
DIGEST = re.compile(r'^sha256:[0-9a-f]{64}$')
ACCEPT = ', '.join(['application/vnd.oci.image.index.v1+json',
                    'application/vnd.docker.distribution.manifest.list.v2+json',
                    'application/vnd.oci.image.manifest.v1+json',
                    'application/vnd.docker.distribution.manifest.v2+json'])


class SafeRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if urllib.parse.urlsplit(newurl).scheme != 'https':
            raise RuntimeError('Refusing a non-HTTPS registry redirect')
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if urllib.parse.urlsplit(req.full_url).netloc != urllib.parse.urlsplit(newurl).netloc:
            redirected.remove_header('Authorization')
        return redirected


class Registry:
    def __init__(self):
        self.open = urllib.request.build_opener(SafeRedirect()).open
        actor, token = os.environ['GITHUB_ACTOR'], os.environ['GITHUB_TOKEN']
        basic = base64.b64encode(f'{actor}:{token}'.encode()).decode()
        url = 'https://ghcr.io/token?' + urllib.parse.urlencode({
            'service': 'ghcr.io', 'scope': f'repository:{REPOSITORY}:pull'})
        try:
            with self.open(urllib.request.Request(url, headers={'Authorization': 'Basic ' + basic}), timeout=30) as response:
                payload = json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f'Registry authentication failed: HTTP {error.code}') from None
        self.token = payload.get('token') or payload.get('access_token')
        if not self.token:
            raise RuntimeError('Registry returned no scoped access token')

    def get(self, kind, reference, allow_missing=False):
        if kind not in ('manifests', 'blobs') or not (DIGEST.fullmatch(reference) or reference in ('v0.1.0', 'latest')):
            raise RuntimeError('Invalid registry resource')
        url = f'https://ghcr.io/v2/{REPOSITORY}/{kind}/{reference}'
        request = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + self.token, 'Accept': ACCEPT})
        try:
            with self.open(request, timeout=30) as response:
                raw = response.read()
                claimed = response.headers.get('Docker-Content-Digest')
        except urllib.error.HTTPError as error:
            # A valid token alone is insufficient: only this explicit manifest error means absent.
            if error.code == 404 and allow_missing and kind == 'manifests':
                try:
                    errors = json.load(error).get('errors', [])
                except (ValueError, AttributeError):
                    errors = []
                if errors and all(item.get('code') in ('MANIFEST_UNKNOWN', 'NAME_UNKNOWN') for item in errors):
                    return None
            raise RuntimeError(f'Registry read failed for {kind}/{reference}: HTTP {error.code}; stop without publishing') from None
        digest = 'sha256:' + hashlib.sha256(raw).hexdigest()
        if claimed and claimed != digest:
            raise RuntimeError('Registry digest header does not match response bytes')
        if DIGEST.fullmatch(reference) and digest != reference:
            raise RuntimeError('Registry content does not match requested digest')
        return json.loads(raw), digest


def check_index(registry, result):
    index, digest = result
    manifests = index.get('manifests', [])
    platforms = [(x.get('platform', {}).get('os'), x.get('platform', {}).get('architecture')) for x in manifests]
    if sorted(platforms) != [('linux', 'amd64'), ('linux', 'arm64')]:
        raise RuntimeError('Index must contain exactly native linux/amd64 and linux/arm64 images')
    for descriptor in manifests:
        manifest, _ = registry.get('manifests', descriptor['digest'])
        config, _ = registry.get('blobs', manifest['config']['digest'])
        platform = descriptor['platform']
        if config.get('architecture') != platform['architecture'] or config.get('os') != 'linux':
            raise RuntimeError('Image configuration platform does not match index')
        if config.get('config', {}).get('Labels', {}).get('org.opencontainers.image.revision') != SOURCE:
            raise RuntimeError('Image source revision does not match v0.1.0')
    return digest


def preflight(registry):
    existing = []
    # Inspect BOTH references before reporting; never interpret a denied request as absence.
    for tag in ('v0.1.0', 'latest'):
        result = registry.get('manifests', tag, allow_missing=True)
        if result:
            _, digest = result
            try:
                check_index(registry, result)
                state = 'verified exact release source and both platforms'
            except (RuntimeError, KeyError, TypeError) as error:
                state = f'not verified: {error}'
            existing.append({'tag': tag, 'digest': digest, 'state': state})
    if existing:
        print(json.dumps({'existing': existing}, sort_keys=True))
        raise RuntimeError('Existing image tag found; no overwrite or replay. Review the reported digests.')
    print('Authenticated registry checks: both v0.1.0 and latest are absent')


def verify(registry):
    digests = {tag: check_index(registry, registry.get('manifests', tag)) for tag in ('v0.1.0', 'latest')}
    if len(set(digests.values())) != 1:
        raise RuntimeError('Version and latest point to different indexes')
    print(json.dumps({'source': SOURCE, 'platforms': ['linux/amd64', 'linux/arm64'], 'digests': digests}, sort_keys=True))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('mode', choices=['preflight', 'verify'])
    args = parser.parse_args()
    try:
        {'preflight': preflight, 'verify': verify}[args.mode](Registry())
    except (RuntimeError, KeyError, ValueError, TypeError, urllib.error.URLError) as error:
        # Never print request objects, authentication headers, or raw response bodies.
        print(f'Recovery stopped: {error}')
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

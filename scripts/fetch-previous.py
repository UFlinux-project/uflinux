#!/usr/bin/env python3
"""Fetch the published repository history before replacing the Pages site."""
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.error
import urllib.request

from builder import ARCHES, repository_url


def open_repository_url(url, timeout):
    for retry_delay in (5, 10, 20, None):
        try:
            return urllib.request.urlopen(url, timeout=timeout)
        except urllib.error.HTTPError as error:
            if retry_delay is None or error.code not in (429, 500, 502, 503, 504):
                error.add_note(f'While fetching {url}')
                raise
            error.close()
            print(f'HTTP {error.code} fetching {url}. Retrying in {retry_delay}s.', flush=True)
            time.sleep(retry_delay)


repository = repository_url(os.environ.get('APK_REPOSITORY_URL'))
destination = Path('out/previous/repository')
try:
    with open_repository_url(repository + '/releases.json', timeout=30) as response:
        manifest = response.read(1024 * 1024 + 1)
except urllib.error.HTTPError as error:
    if error.code != 404:
        raise
    # A missing Pages repository is only acceptable before the first release.
    result = subprocess.run(['gh', 'api', 'repos/{}/releases/latest'.format(os.environ['GITHUB_REPOSITORY'])],
                            text=True, capture_output=True)
    if result.returncode == 0 or 'HTTP 404' not in result.stderr:
        raise RuntimeError('Published repository history is unavailable: ' + result.stderr) from error
else:
    if len(manifest) > 1024 * 1024:
        raise ValueError('Repository history exceeds its size limit')
    history = json.loads(manifest)
    if not isinstance(history, list) or not history:
        raise ValueError('Published repository history is empty or invalid')
    destination.mkdir(parents=True, exist_ok=True)
    (destination / 'releases.json').write_bytes(manifest)
    for arch in ARCHES:
        target = destination / arch
        target.mkdir(exist_ok=True)
        for name in {name for release in history for name in release['packages'][arch]}:
            if not isinstance(name, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._+~-]*\.apk', name):
                raise ValueError('Invalid APK snapshot filename')
            with open_repository_url(repository + '/' + arch + '/' + name, timeout=60) as response:
                with (target / name).open('wb') as output:
                    while chunk := response.read(1024 * 1024):
                        output.write(chunk)

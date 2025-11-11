#!/bin/sh

# Generate a file that tracks the UI version
# This helps Docker detect when the UI submodule has changed

set -e

UI_VERSION=$(git -C ui describe --tags 2>/dev/null || echo "unknown")
UI_COMMIT=$(git -C ui rev-parse HEAD 2>/dev/null || echo "unknown")

echo "UI_VERSION=${UI_VERSION}" > ui-version.env
echo "UI_COMMIT=${UI_COMMIT}" >> ui-version.env

echo "Generated ui-version.env with UI_VERSION=${UI_VERSION} UI_COMMIT=${UI_COMMIT}"
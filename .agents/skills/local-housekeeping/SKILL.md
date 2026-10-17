---
name: project-housekeeping-scan
description: Scans a project for common maintenance tasks including dependency updates, stale documentation, outdated configurations, and security issues. Activates when asked to audit, check maintenance needs, or identify housekeeping tasks.
---

# Project Housekeeping Scan

## When to Apply

- User asks to scan or audit the project for maintenance tasks
- User requests a housekeeping check or cleanup recommendations
- User wants to identify outdated dependencies or configurations
- User asks what needs updating or maintaining in the project
- User requests a technical debt or maintenance report

## Steps

1. **Scan Package Dependencies**
   - Identify which package managers are in use by checking for their manifest files:
     - `package.json` (npm/yarn/pnpm)
     - `composer.json` (Composer)
     - `requirements.txt`, `Pipfile`, or `pyproject.toml` (pip/pipenv/poetry)
     - `Gemfile` (Bundler)
     - `go.mod` (Go modules)
     - `Cargo.toml` (Cargo)
     - `pubspec.yaml` (Dart/Flutter)
   - For each detected package manager, determine how to check for updates:
     - npm: run `npm outdated` or check against npm registry
     - Composer: run `composer outdated` or check Packagist
     - pip/poetry: check PyPI for newer versions
     - Bundler: run `bundle outdated`
     - Go: run `go list -u -m all`
     - Cargo: run `cargo outdated`
   - Report packages with available updates, noting major vs minor vs patch versions
   - Flag any dependencies with known security vulnerabilities
   - Identify packages that are deprecated or unmaintained
   - Check for abandoned packages that have recommended replacements

2. **Review Documentation Staleness**
   - Check README.md last modified date against recent code changes
   - Identify installation or setup instructions that reference outdated versions
   - Check if API documentation matches current function signatures
   - Flag missing documentation for new features (compare recent commits)
   - Verify links in markdown files are not broken
   - Check for placeholder text that was never replaced
   - Review changelog or release notes for completeness

3. **Audit Configuration Files**
   - Check runtime version specifications across the project:
     - Node.js version in `.nvmrc`, `package.json` engines, or CI configs
     - PHP version in `composer.json` or deployment configs
     - Python version in `.python-version` or `pyproject.toml`
     - Ruby version in `.ruby-version` or `Gemfile`
   - Verify database versions in `docker-compose.yml` or deployment docs
   - Check if `.env.example` matches current `.env` requirements
   - Review CI/CD configs (GitHub Actions, GitLab CI, CircleCI) for deprecated actions or outdated runner versions
   - Check for outdated syntax or deprecated features in configuration files

4. **Review Test Coverage**
   - Check if test suite exists and runs successfully
   - Identify critical files without test coverage
   - Flag tests that are skipped or marked as pending
   - Check for outdated test fixtures or mock data
   - Verify test database seeds are current

5. **Check Security and Compliance**
   - Look for hardcoded secrets, API keys, or passwords in code
   - Check for `.env` files accidentally committed to git
   - Verify `.gitignore` includes common sensitive files
   - Check SSL certificate expiry dates in configs
   - Review CORS or security header configurations
   - Check for exposed debug endpoints or development routes

6. **Examine Build and Deployment**
   - Check if build scripts are documented and functional
   - Verify deployment instructions are current
   - Look for unused build artifacts or cache directories
   - Check if Docker images reference outdated base images
   - Review environment-specific configs for consistency
   - Verify build dependencies match runtime requirements

7. **Assess Repository Health**
   - Check for large files that should be in Git LFS
   - Identify unused or stale branches (no commits in 6+ months)
   - Review open issues and PRs for actionable maintenance items
   - Check if `.gitattributes` is properly configured
   - Verify LICENSE file exists and is appropriate
   - Look for binary files that shouldn't be committed

8. **Report Findings**
   - Group findings by priority (critical, important, minor)
   - Provide specific file paths and line numbers
   - Suggest concrete next actions for each item
   - Estimate effort required for each maintenance task
   - Highlight quick wins that can be addressed immediately
   - Organize by category for easier triage
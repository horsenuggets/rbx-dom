#
# main.tf
#
# Manages this repository's GitHub settings, including merge options and the ruleset that
# protects the default branch with the required CI checks. Run plans and applies from this
# folder with GITHUB_TOKEN set, for example "GITHUB_TOKEN=$(gh auth token) terraform plan".
#

terraform {
    required_version = ">= 1.5"

    # State lives in HCP Terraform. The organization comes from the TF_CLOUD_ORGANIZATION
    # environment variable, and plans run locally.
    cloud {
        workspaces {
            name = "rbx-dom"
        }
    }

    required_providers {
        github = {
            source = "integrations/github"
            version = "~> 6.0"
        }
    }
}

provider "github" {
    # Authenticates via the GITHUB_TOKEN environment variable.
    owner = "horsenuggets"
}

locals {
    repo_name = "rbx-dom"

    # Must match the job names reported by the existing workflows in .github/workflows/.
    main_checks = [
        "build (stable)",
        "lint (stable)",
        "validate"
    ]
}

# Adopt the existing repository instead of trying to create it. The id must be a literal
# string on Terraform 1.5, so keep it in sync with local.repo_name.
import {
    to = github_repository.repo
    id = "rbx-dom"
}

resource "github_repository" "repo" {
    name = local.repo_name

    has_issues = true
    has_projects = false
    has_wiki = false

    allow_squash_merge = true
    allow_rebase_merge = false

    # Upstream syncs land with a merge commit, which keeps upstream history on this fork.
    allow_merge_commit = true

    squash_merge_commit_title = "PR_TITLE"
    squash_merge_commit_message = "PR_BODY"

    delete_branch_on_merge = true

    lifecycle {
        # Destroying this resource would delete the repository on GitHub.
        prevent_destroy = true

        # Managed in the GitHub UI, so applies don't overwrite them.
        ignore_changes = [allow_auto_merge, description, homepage_url, topics, visibility]
    }
}

# GitHub disables workflows on new forks, but this fork's own pull requests run them.
resource "github_actions_repository_permissions" "repo" {
    repository = github_repository.repo.name
    enabled = true
    allowed_actions = "all"
}

resource "github_repository_ruleset" "main" {
    name = "main"
    repository = github_repository.repo.name
    target = "branch"
    enforcement = "active"

    conditions {
        ref_name {
            include = ["~DEFAULT_BRANCH"]
            exclude = []
        }
    }

    rules {
        non_fast_forward = true
        deletion = true

        pull_request {
            required_approving_review_count = 0
            dismiss_stale_reviews_on_push = true
        }

        required_status_checks {
            strict_required_status_checks_policy = true

            dynamic "required_check" {
                for_each = local.main_checks
                content {
                    context = required_check.value
                    # Only GitHub Actions can satisfy this required check.
                    integration_id = 15368
                }
            }
        }
    }
}

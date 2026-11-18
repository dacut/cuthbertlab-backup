variable "name" {
  description = "Name given to the Lambda function, state machine, and related resources."
  type        = string
  default     = "cuthbertlab-backup"
}

variable "region" {
  description = "AWS region to deploy into. Must be the region the bucket is in."
  type        = string
  default     = "us-west-2"
}

variable "tags" {
  description = "Extra tags for every resource."
  type        = map(string)
  default     = {}
}

# --- What to back up, and where to ---

variable "github_org" {
  description = "GitHub organization whose repositories are backed up."
  type        = string
  default     = "cuthbertLab"
}

variable "github_token_parameter" {
  description = "Name of an existing SSM SecureString parameter, in this region, that holds the GitHub token. Terraform does not create or read it."
  type        = string
  default     = "/cuthbertlab-backup/github-token"

  validation {
    condition     = startswith(var.github_token_parameter, "/") && !startswith(var.github_token_parameter, "arn:")
    error_message = "Give the parameter's full name, starting with \"/\" (not its ARN)."
  }
}

variable "bucket_name" {
  description = "S3 bucket that receives the backups."
  type        = string
  default     = "cuthbertlab-backup"
}

variable "import_existing_bucket" {
  description = <<-EOT
    Adopt an S3 bucket that already exists instead of creating one. The
    cuthbertlab-backup bucket predates this configuration, so this defaults to
    true. Set it to false when deploying to a bucket name that does not exist yet.
  EOT
  type        = bool
  default     = true
}

variable "bucket_prefix" {
  description = "Key prefix for everything this backup writes, leaving room in the bucket for other things."
  type        = string
  default     = "github"
}

variable "backup_projects" {
  description = <<-EOT
    Back up the organization's Projects (v2). This needs a token with the
    read:project scope; without it the organization task fails, on purpose,
    so that a missing permission is noticed. Set to false to leave projects out.
  EOT
  type        = bool
  default     = true
}

variable "backup_attachments" {
  description = "Download images and files attached to issues, pull requests, comments, discussions and release notes."
  type        = bool
  default     = true
}

# --- Retention ---

variable "noncurrent_version_retention_days" {
  description = <<-EOT
    How long a superseded version of a backup object is kept. Every changed file
    is written as a new version, so this is how far back you can go to recover
    from, say, a force-push or a deleted issue.
  EOT
  type        = number
  default     = 90

  validation {
    condition     = var.noncurrent_version_retention_days >= 1
    error_message = "Must be at least 1 day."
  }
}

variable "noncurrent_versions_always_kept" {
  description = "This many of the most recent superseded versions of each object are kept regardless of age."
  type        = number
  default     = 3

  validation {
    condition     = var.noncurrent_versions_always_kept >= 0 && var.noncurrent_versions_always_kept <= 100
    error_message = "Must be between 0 and 100."
  }
}

# --- Schedule ---

variable "schedule" {
  description = "When to run, as an EventBridge Scheduler expression."
  type        = string
  default     = "cron(0 3 * * ? *)"
}

variable "schedule_timezone" {
  description = "Time zone the schedule is expressed in."
  type        = string
  default     = "America/Los_Angeles"
}

variable "schedule_enabled" {
  description = "Set to false to deploy everything but not run on a schedule (for example, until the GitHub token is in place)."
  type        = bool
  default     = true
}

# --- Alerting ---

variable "alert_email" {
  description = "Address to email when a backup run fails or backups stop succeeding. The recipient must confirm the subscription. Leave null to create the SNS topic without a subscriber."
  type        = string
  default     = null
}

variable "stale_backup_alarm_days" {
  description = <<-EOT
    Alarm if this many days pass without a fully successful run. This catches
    what a failure notification cannot: a schedule that silently stopped firing.
    Between 1 and 7; 0 disables the alarm (do that if the schedule is less
    frequent than daily).
  EOT
  type        = number
  default     = 2

  validation {
    condition     = var.stale_backup_alarm_days >= 0 && var.stale_backup_alarm_days <= 7 && floor(var.stale_backup_alarm_days) == var.stale_backup_alarm_days
    error_message = "Must be a whole number from 0 to 7."
  }
}

# --- Sizing ---

variable "lambda_zip" {
  description = "Path to the built function package. Defaults to where `make build` (cargo lambda) puts it."
  type        = string
  default     = null
}

variable "lambda_memory_mb" {
  description = "Function memory. Also determines CPU, which is what a large git fetch is limited by."
  type        = number
  default     = 4096
}

variable "lambda_ephemeral_storage_mb" {
  description = "Scratch disk. A git mirror needs about twice the repository's size."
  type        = number
  default     = 8192
}

variable "max_concurrency" {
  description = "How many backup tasks run at once. All of them share one GitHub token's rate limit."
  type        = number
  default     = 4
}

variable "log_retention_days" {
  description = "How long to keep the function's CloudWatch logs."
  type        = number
  default     = 90
}

variable "log_level" {
  description = "Function log level: error, warn, info, debug, or trace."
  type        = string
  default     = "info"
}

variable "website_domain" {
  description = "Host name the bucket is published at, through CloudFront."
  type        = string
  default     = "cuthbertlab-backup.kanga.org"
}

variable "website_zone" {
  description = "Public Route 53 hosted zone, in this account, that website_domain belongs to."
  type        = string
  default     = "kanga.org"
}

variable "website_title" {
  description = "Shown at the top of every directory listing. Set to null to show website_domain."
  type        = string
  default     = "cuthbertLab GitHub backup"
}

variable "website_file_ttl_seconds" {
  description = "How long CloudFront and browsers may keep a file before checking for a newer one."
  type        = number
  default     = 3600
}

variable "listing_lambda_zip" {
  description = "Path to the directory listing function's package. Defaults to where `make build` puts it."
  type        = string
  default     = null
}

variable "website_price_class" {
  description = "CloudFront price class: PriceClass_All, PriceClass_200 or PriceClass_100."
  type        = string
  default     = "PriceClass_All"
}

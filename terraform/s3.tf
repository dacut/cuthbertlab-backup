# The backup bucket.
#
# The bucket already existed when this configuration was written, so it is
# adopted rather than created. Once it is in the state the import block does
# nothing and can stay.
import {
  for_each = var.import_existing_bucket ? toset([var.bucket_name]) : toset([])
  to       = aws_s3_bucket.backup
  id       = each.value
}

resource "aws_s3_bucket" "backup" {
  bucket = var.bucket_name

  lifecycle {
    # `terraform destroy` must never take the backups with it.
    prevent_destroy = true
  }
}

# Versioning is what turns "the latest copy" into a backup: every overwrite
# keeps the previous version, so damage upstream (a force-push, a deleted
# issue, a compromised account) cannot silently replace good data with bad.
resource "aws_s3_bucket_versioning" "backup" {
  bucket = aws_s3_bucket.backup.id

  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "backup" {
  bucket = aws_s3_bucket.backup.id

  # Lifecycle rules act on versions, so versioning has to be on first.
  depends_on = [aws_s3_bucket_versioning.backup]

  rule {
    id     = "expire-old-versions"
    status = "Enabled"

    filter {}

    # Current versions are never expired. Superseded ones are kept for a
    # while, and the most recent few are kept indefinitely.
    noncurrent_version_expiration {
      noncurrent_days           = var.noncurrent_version_retention_days
      newer_noncurrent_versions = var.noncurrent_versions_always_kept > 0 ? var.noncurrent_versions_always_kept : null
    }

    # Parts of large uploads that were interrupted are invisible but billed.
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "backup" {
  bucket = aws_s3_bucket.backup.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "backup" {
  bucket = aws_s3_bucket.backup.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "backup" {
  bucket = aws_s3_bucket.backup.id

  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_policy" "backup" {
  bucket = aws_s3_bucket.backup.id

  depends_on = [aws_s3_bucket_public_access_block.backup]

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "DenyInsecureTransport"
        Effect    = "Deny"
        Principal = "*"
        Action    = "s3:*"
        Resource  = [aws_s3_bucket.backup.arn, "${aws_s3_bucket.backup.arn}/*"]
        Condition = { Bool = { "aws:SecureTransport" = "false" } }
      },
      {
        # The website (website.tf). Naming the distribution keeps the bucket
        # private to everything else, including other CloudFront distributions,
        # so the public access block above stays fully on. s3:GetObject alone
        # reaches current versions only, and without s3:ListBucket a missing
        # key reads as 403, not 404.
        Sid       = "AllowWebsiteDistribution"
        Effect    = "Allow"
        Principal = { Service = "cloudfront.amazonaws.com" }
        Action    = "s3:GetObject"
        Resource  = "${aws_s3_bucket.backup.arn}/*"
        Condition = { StringEquals = { "AWS:SourceArn" = aws_cloudfront_distribution.website.arn } }
      }
    ]
  })
}

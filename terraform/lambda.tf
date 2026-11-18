# The backup function: one Rust binary that handles every kind of task.

locals {
  lambda_zip = coalesce(var.lambda_zip, "${path.module}/../target/lambda/cuthbertlab-backup/bootstrap.zip")
}

resource "aws_cloudwatch_log_group" "lambda" {
  name              = "/aws/lambda/${var.name}"
  retention_in_days = var.log_retention_days
}

resource "aws_lambda_function" "backup" {
  function_name = var.name
  description   = "Backs up the ${var.github_org} GitHub organization to s3://${var.bucket_name}/${local.prefix}"
  role          = aws_iam_role.lambda.arn

  runtime       = "provided.al2023"
  handler       = "bootstrap"
  architectures = ["arm64"]

  filename         = local.lambda_zip
  source_code_hash = fileexists(local.lambda_zip) ? filebase64sha256(local.lambda_zip) : null

  # The maximum. Each invocation handles one task, and the state machine
  # retries a task that runs out of time; the slow parts resume where they
  # stopped.
  timeout     = 900
  memory_size = var.lambda_memory_mb

  ephemeral_storage {
    size = var.lambda_ephemeral_storage_mb
  }

  environment {
    variables = {
      GITHUB_ORG             = var.github_org
      GITHUB_TOKEN_PARAMETER = var.github_token_parameter
      BACKUP_BUCKET          = aws_s3_bucket.backup.bucket
      BACKUP_PREFIX          = local.prefix
      BACKUP_PROJECTS        = tostring(var.backup_projects)
      BACKUP_ATTACHMENTS     = tostring(var.backup_attachments)
      LOG_LEVEL              = var.log_level
    }
  }

  logging_config {
    log_format = "Text" # the function emits JSON lines itself
    log_group  = aws_cloudwatch_log_group.lambda.name
  }

  depends_on = [aws_iam_role_policy.lambda]

  lifecycle {
    precondition {
      condition     = fileexists(local.lambda_zip)
      error_message = "The function package ${local.lambda_zip} does not exist. Build it first with `make build`."
    }
  }
}

resource "aws_iam_role" "lambda" {
  name        = "${var.name}-lambda"
  description = "Execution role of the ${var.name} function"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "sts:AssumeRole"
      Principal = { Service = "lambda.amazonaws.com" }
    }]
  })
}

resource "aws_iam_role_policy" "lambda" {
  name = "backup"
  role = aws_iam_role.lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "WriteLogs"
        Effect   = "Allow"
        Action   = ["logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "${aws_cloudwatch_log_group.lambda.arn}:*"
      },
      {
        Sid      = "ReadGitHubToken"
        Effect   = "Allow"
        Action   = "ssm:GetParameter"
        Resource = local.github_token_parameter_arn
      },
      {
        # Needed for more than listing: without it, S3 answers a HEAD for a
        # missing object with 403 instead of 404, and "not backed up yet"
        # becomes indistinguishable from "not allowed". It must not be
        # narrowed with an s3:prefix condition, either: a HEAD request has no
        # prefix, so such a condition never matches and the 403 comes back.
        Sid      = "ListBackups"
        Effect   = "Allow"
        Action   = "s3:ListBucket"
        Resource = aws_s3_bucket.backup.arn
      },
      {
        # Deliberately no s3:DeleteObject or s3:DeleteObjectVersion. The
        # function can add versions but cannot remove any, so neither a bug
        # nor a stolen credential can destroy backup history.
        Sid      = "ReadAndWriteBackups"
        Effect   = "Allow"
        Action   = ["s3:GetObject", "s3:PutObject", "s3:AbortMultipartUpload"]
        Resource = "${aws_s3_bucket.backup.arn}/${local.prefix}*"
      },
    ]
  })
}

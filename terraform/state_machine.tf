# The orchestration: plan, then run every task, then report.
#
#   Plan ──▶ Backup (Map: one Lambda invocation per task) ──▶ Check ──▶ Succeeded
#                                                                └────▶ Failed
#
# A task that fails is retried, and if it keeps failing it is recorded while
# the others carry on: one broken repository must not stop the rest from being
# backed up. The execution as a whole then ends as Failed, which is what the
# alerting keys off.

locals {
  # Errors from the Lambda service itself, as opposed to from the function.
  lambda_service_errors = [
    "Lambda.ServiceException",
    "Lambda.AWSLambdaException",
    "Lambda.SdkClientException",
    "Lambda.TooManyRequestsException",
  ]

  task_retries = [
    {
      ErrorEquals     = local.lambda_service_errors
      IntervalSeconds = 2
      MaxAttempts     = 6
      BackoffRate     = 2
      JitterStrategy  = "FULL"
    },
    {
      # The function failed or timed out. Worth another go: GitHub hiccups
      # pass, and binaries already uploaded are not transferred again.
      ErrorEquals     = ["States.ALL"]
      IntervalSeconds = 60
      MaxAttempts     = 2
      BackoffRate     = 2
    },
  ]

  state_machine_definition = {
    Comment       = "Back up the ${var.github_org} GitHub organization to S3"
    QueryLanguage = "JSONata"
    StartAt       = "Plan"
    # Generous: a first run uploads everything, and each task may take
    # three 15-minute attempts.
    TimeoutSeconds = 6 * 60 * 60

    States = {
      Plan = {
        Type     = "Task"
        Comment  = "List the organization's repositories and turn them into tasks"
        Resource = "arn:${local.partition}:states:::lambda:invoke"
        Arguments = {
          FunctionName = aws_lambda_function.backup.arn
          Payload      = { action = "plan" }
        }
        Output = "{% $states.result.Payload %}"
        Retry  = local.task_retries
        Next   = "Backup"
      }

      Backup = {
        Type           = "Map"
        Comment        = "Run every task, a few at a time"
        Items          = "{% $states.input.tasks %}"
        MaxConcurrency = var.max_concurrency

        ItemProcessor = {
          ProcessorConfig = { Mode = "INLINE" }
          StartAt         = "RunTask"

          States = {
            RunTask = {
              Type           = "Task"
              Resource       = "arn:${local.partition}:states:::lambda:invoke"
              TimeoutSeconds = 930
              Arguments = {
                FunctionName = aws_lambda_function.backup.arn
                Payload      = "{% $states.input %}"
              }
              # Keep only the headline: full results are in the function's
              # logs and in each manifest.json, and the combined state of a
              # Map is limited to 256 KiB.
              Output = {
                task     = "{% $states.result.Payload.task %}"
                status   = "{% $states.result.Payload.status %}"
                warnings = "{% $count($states.result.Payload.warnings) %}"
              }
              Retry = local.task_retries
              Catch = [{
                ErrorEquals = ["States.ALL"]
                Output = {
                  task   = "{% $states.input %}"
                  status = "failed"
                  error  = "{% $states.errorOutput.Error %}"
                  cause  = "{% $substring($string($states.errorOutput.Cause), 0, 1500) %}"
                }
                Next = "TaskFailed"
              }]
              End = true
            }

            TaskFailed = {
              Type    = "Pass"
              Comment = "Record the failure and let the other tasks continue"
              End     = true
            }
          }
        }

        Output = {
          org       = "{% $states.input.org %}"
          tasks     = "{% $count($states.result) %}"
          succeeded = "{% $count($states.result[status = 'ok']) %}"
          warnings  = "{% $sum($states.result.warnings) %}"
          failed    = "{% [$states.result[status = 'failed']] %}"
        }
        Next = "Check"
      }

      Check = {
        Type = "Choice"
        Choices = [{
          Condition = "{% $count($states.input.failed) > 0 %}"
          Next      = "Failed"
        }]
        Default = "Succeeded"
      }

      Failed = {
        Type  = "Fail"
        Error = "BackupIncomplete"
        Cause = "{% $string($count($states.input.failed)) & ' of ' & $string($states.input.tasks) & ' backup tasks failed: ' & $join($states.input.failed.(task.part & ($exists(task.repo) ? ':' & task.repo : '') & ' (' & $substring(cause, 0, 400) & ')'), '; ') %}"
      }

      Succeeded = {
        Type = "Succeed"
      }
    }
  }
}

resource "aws_sfn_state_machine" "backup" {
  name       = var.name
  role_arn   = aws_iam_role.state_machine.arn
  definition = jsonencode(local.state_machine_definition)
}

resource "aws_iam_role" "state_machine" {
  name        = "${var.name}-state-machine"
  description = "Lets the ${var.name} state machine invoke its function"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "sts:AssumeRole"
      Principal = { Service = "states.amazonaws.com" }
      Condition = { StringEquals = { "aws:SourceAccount" = local.account_id } }
    }]
  })
}

resource "aws_iam_role_policy" "state_machine" {
  name = "invoke-function"
  role = aws_iam_role.state_machine.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = "lambda:InvokeFunction"
      Resource = [aws_lambda_function.backup.arn, "${aws_lambda_function.backup.arn}:*"]
    }]
  })
}

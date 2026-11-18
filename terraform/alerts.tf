# Telling someone when backups are not happening.
#
# Two independent signals feed one SNS topic:
#
#   1. A run ended badly (a task failed, or the run itself broke).
#   2. No run has fully succeeded lately. This is the one that matters most:
#      it also covers a schedule that stopped firing, which by its nature
#      produces no failure to notify about.

resource "aws_sns_topic" "alerts" {
  name = "${var.name}-alerts"
}

resource "aws_sns_topic_subscription" "email" {
  count = var.alert_email == null ? 0 : 1

  topic_arn = aws_sns_topic.alerts.arn
  protocol  = "email"
  endpoint  = var.alert_email
}

resource "aws_sns_topic_policy" "alerts" {
  arn = aws_sns_topic.alerts.arn

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "AllowEventBridge"
        Effect    = "Allow"
        Principal = { Service = "events.amazonaws.com" }
        Action    = "sns:Publish"
        Resource  = aws_sns_topic.alerts.arn
        Condition = { ArnEquals = { "aws:SourceArn" = aws_cloudwatch_event_rule.run_failed.arn } }
      },
      {
        Sid       = "AllowCloudWatchAlarms"
        Effect    = "Allow"
        Principal = { Service = "cloudwatch.amazonaws.com" }
        Action    = "sns:Publish"
        Resource  = aws_sns_topic.alerts.arn
        Condition = { StringEquals = { "aws:SourceAccount" = local.account_id } }
      },
    ]
  })
}

# 1. A run ended badly.

resource "aws_cloudwatch_event_rule" "run_failed" {
  name        = "${var.name}-run-failed"
  description = "A ${var.name} run failed, timed out, or was aborted"

  event_pattern = jsonencode({
    source      = ["aws.states"]
    detail-type = ["Step Functions Execution Status Change"]
    detail = {
      stateMachineArn = [aws_sfn_state_machine.backup.arn]
      status          = ["FAILED", "TIMED_OUT", "ABORTED"]
    }
  })
}

# The event is delivered as is. It is JSON rather than prose, but it carries
# everything needed: `status`, `error`, the execution ARN, and a `cause` that
# the state machine writes to name each failed task and why it failed.
resource "aws_cloudwatch_event_target" "run_failed" {
  rule = aws_cloudwatch_event_rule.run_failed.name
  arn  = aws_sns_topic.alerts.arn
}

# 2. No run has fully succeeded lately.

resource "aws_cloudwatch_metric_alarm" "stale" {
  count = var.stale_backup_alarm_days > 0 ? 1 : 0

  alarm_name        = "${var.name}-no-recent-success"
  alarm_description = "No ${var.github_org} GitHub backup run has fully succeeded in the last ${var.stale_backup_alarm_days} day(s)."

  namespace   = "AWS/States"
  metric_name = "ExecutionsSucceeded"
  dimensions  = { StateMachineArn = aws_sfn_state_machine.backup.arn }

  statistic           = "Sum"
  period              = 86400
  evaluation_periods  = var.stale_backup_alarm_days
  comparison_operator = "LessThanThreshold"
  threshold           = 1

  # No executions at all means no data points, which is exactly the
  # situation this alarm exists for.
  treat_missing_data = "breaching"

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]
}

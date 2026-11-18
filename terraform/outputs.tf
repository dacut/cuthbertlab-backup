output "bucket" {
  description = "Where the backups are."
  value       = "s3://${aws_s3_bucket.backup.bucket}/${local.prefix}"
}

output "state_machine_arn" {
  description = "The state machine that performs a backup run."
  value       = aws_sfn_state_machine.backup.arn
}

output "function_name" {
  description = "The Lambda function that performs each backup task."
  value       = aws_lambda_function.backup.function_name
}

output "github_token_parameter" {
  description = "Name of the SSM parameter that must hold the GitHub token. Not managed by Terraform."
  value       = var.github_token_parameter
}

output "alerts_topic_arn" {
  description = "SNS topic that failure and staleness alerts are sent to."
  value       = aws_sns_topic.alerts.arn
}

output "set_token_command" {
  description = "Stores the GitHub token. Run to create the parameter, and again whenever the token is rotated."
  value       = "aws ssm put-parameter --region ${var.region} --overwrite --type SecureString --name ${var.github_token_parameter} --value \"$GITHUB_TOKEN\""
}

output "run_now_command" {
  description = "Starts a backup run immediately."
  value       = "aws stepfunctions start-execution --region ${var.region} --state-machine-arn ${aws_sfn_state_machine.backup.arn}"
}

output "website_url" {
  description = "Where the backups can be browsed."
  value       = "https://${var.website_domain}/"
}

output "website_distribution_id" {
  description = "The CloudFront distribution serving the website."
  value       = aws_cloudfront_distribution.website.id
}

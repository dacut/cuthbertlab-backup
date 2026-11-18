# The GitHub token the function authenticates with.
#
# It lives in an SSM SecureString parameter that is created and rotated outside
# Terraform:
#
#   aws ssm put-parameter --overwrite --type SecureString \
#     --name /cuthbertlab-backup/github-token --value "$TOKEN"
#
# Terraform knows the parameter only by name (var.github_token_parameter). There
# is deliberately no resource and no data source for it, so its value can never
# reach the state file. Do not add an `aws_ssm_parameter` data source to look up
# the ARN: reading one stores the decrypted value in state.
#
# The function reads the parameter at cold start, so a new value is picked up
# by the next run.
locals {
  # The parameter is expected to use the account's AWS-managed key
  # (alias/aws/ssm), whose key policy lets any principal allowed to read the
  # parameter decrypt it through SSM. With a customer-managed key the function
  # would also need kms:Decrypt on that key.
  github_token_parameter_arn = "arn:${local.partition}:ssm:${var.region}:${local.account_id}:parameter/${trimprefix(var.github_token_parameter, "/")}"
}

terraform {
  required_version = ">= 1.7"

  # Partial configuration: supply the rest with
  #   terraform init -backend-config=kanga.s3.tfbackend
  backend "s3" {}

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
}

provider "aws" {
  region = var.region

  default_tags {
    tags = merge({ Project = var.name }, var.tags)
  }
}

data "aws_caller_identity" "current" {}
data "aws_partition" "current" {}

locals {
  account_id = data.aws_caller_identity.current.account_id
  partition  = data.aws_partition.current.partition

  # Empty, or ending in "/". The function normalizes it the same way.
  prefix = trim(var.bucket_prefix, "/") == "" ? "" : "${trim(var.bucket_prefix, "/")}/"
}

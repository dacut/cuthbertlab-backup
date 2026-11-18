# Publishes the backup bucket as a browsable website at var.website_domain.
#
#   /path/to/file    S3, through CloudFront. If S3 has no such object (it
#                    answers 403, as CloudFront may not list the bucket), the
#                    origin group retries at the listing function, which
#                    redirects "/dir" to "/dir/" or answers 404.
#   / and */         the listing function (listing/ in this repository), which
#                    renders the directory as HTML or JSON.
#   /_listing/*      the listing function's stylesheet.
#
# Security headers come from a response headers policy, and *.json.gz files
# are marked as downloads by a CloudFront Function (download.js).
#
# The bucket itself stays private. Only this distribution can read objects,
# and only their current versions. The listing function is reachable only
# through this distribution: its URL requires IAM authentication, which
# CloudFront provides with an origin access control.

locals {
  website_s3_origin      = "s3"
  website_listing_origin = "listing"
  website_files_origin   = "s3-then-listing"

  listing_name = "${var.name}-listing"
  listing_zip  = coalesce(var.listing_lambda_zip, "${path.module}/../target/lambda/directory-listing/bootstrap.zip")
}

data "aws_route53_zone" "website" {
  name         = var.website_zone
  private_zone = false
}

# --- Certificate ------------------------------------------------------------

# CloudFront only accepts certificates from us-east-1.
resource "aws_acm_certificate" "website" {
  region = "us-east-1"

  domain_name       = var.website_domain
  validation_method = "DNS"

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_route53_record" "website_certificate_validation" {
  for_each = {
    for option in aws_acm_certificate.website.domain_validation_options : option.domain_name => option
  }

  zone_id         = data.aws_route53_zone.website.zone_id
  name            = each.value.resource_record_name
  type            = each.value.resource_record_type
  records         = [each.value.resource_record_value]
  ttl             = 300
  allow_overwrite = true
}

resource "aws_acm_certificate_validation" "website" {
  region = "us-east-1"

  certificate_arn         = aws_acm_certificate.website.arn
  validation_record_fqdns = [for record in aws_route53_record.website_certificate_validation : record.fqdn]
}

# --- Listing function -------------------------------------------------------

resource "aws_cloudwatch_log_group" "listing" {
  name              = "/aws/lambda/${local.listing_name}"
  retention_in_days = var.log_retention_days
}

resource "aws_lambda_function" "listing" {
  function_name = local.listing_name
  description   = "Directory listings of s3://${aws_s3_bucket.backup.bucket} for https://${var.website_domain}/"
  role          = aws_iam_role.listing.arn

  runtime       = "provided.al2023"
  handler       = "bootstrap"
  architectures = ["arm64"]

  filename         = local.listing_zip
  source_code_hash = fileexists(local.listing_zip) ? filebase64sha256(local.listing_zip) : null

  timeout     = 10
  memory_size = 256

  environment {
    variables = {
      BUCKET     = aws_s3_bucket.backup.bucket
      SITE_TITLE = coalesce(var.website_title, var.website_domain)
      LOG_LEVEL  = var.log_level
    }
  }

  logging_config {
    log_format = "Text" # the function emits JSON lines itself
    log_group  = aws_cloudwatch_log_group.listing.name
  }

  depends_on = [aws_iam_role_policy.listing]

  lifecycle {
    precondition {
      condition     = fileexists(local.listing_zip)
      error_message = "The function package ${local.listing_zip} does not exist. Build it first with `make build`."
    }
  }
}

resource "aws_iam_role" "listing" {
  name        = "${local.listing_name}-lambda"
  description = "Execution role of the ${local.listing_name} function"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Action    = "sts:AssumeRole"
      Principal = { Service = "lambda.amazonaws.com" }
    }]
  })
}

resource "aws_iam_role_policy" "listing" {
  name = "listing"
  role = aws_iam_role.listing.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "WriteLogs"
        Effect   = "Allow"
        Action   = ["logs:CreateLogStream", "logs:PutLogEvents"]
        Resource = "${aws_cloudwatch_log_group.listing.arn}:*"
      },
      {
        # Listing only: the function never reads an object.
        Sid      = "ListBackups"
        Effect   = "Allow"
        Action   = "s3:ListBucket"
        Resource = aws_s3_bucket.backup.arn
      },
    ]
  })
}

resource "aws_lambda_function_url" "listing" {
  function_name      = aws_lambda_function.listing.function_name
  authorization_type = "AWS_IAM"
}

# Calling a function through its URL takes both permissions.
resource "aws_lambda_permission" "listing_url" {
  statement_id           = "AllowCloudFrontFunctionUrl"
  action                 = "lambda:InvokeFunctionUrl"
  function_name          = aws_lambda_function.listing.function_name
  principal              = "cloudfront.amazonaws.com"
  source_arn             = aws_cloudfront_distribution.website.arn
  function_url_auth_type = "AWS_IAM"
}

resource "aws_lambda_permission" "listing_invoke" {
  statement_id             = "AllowCloudFrontInvoke"
  action                   = "lambda:InvokeFunction"
  function_name            = aws_lambda_function.listing.function_name
  principal                = "cloudfront.amazonaws.com"
  source_arn               = aws_cloudfront_distribution.website.arn
  invoked_via_function_url = true
}

# --- CloudFront -------------------------------------------------------------

resource "aws_cloudfront_origin_access_control" "website_s3" {
  name                              = "${var.name}-s3"
  description                       = "Lets ${var.website_domain} read the ${aws_s3_bucket.backup.bucket} bucket"
  origin_access_control_origin_type = "s3"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

resource "aws_cloudfront_origin_access_control" "website_listing" {
  name                              = "${var.name}-listing"
  description                       = "Lets ${var.website_domain} call the ${local.listing_name} function"
  origin_access_control_origin_type = "lambda"
  signing_behavior                  = "always"
  signing_protocol                  = "sigv4"
}

# Files: nothing from the request is passed on, and CloudFront keeps a file
# for var.website_file_ttl_seconds unless the origin says otherwise.
resource "aws_cloudfront_cache_policy" "website_files" {
  name    = "${var.name}-files"
  comment = "Files of ${var.website_domain}"

  min_ttl     = 0
  default_ttl = var.website_file_ttl_seconds
  max_ttl     = 86400

  parameters_in_cache_key_and_forwarded_to_origin {
    enable_accept_encoding_brotli = false
    enable_accept_encoding_gzip   = false

    cookies_config {
      cookie_behavior = "none"
    }
    headers_config {
      header_behavior = "none"
    }
    query_strings_config {
      query_string_behavior = "none"
    }
  }
}

# Listings: HTML or JSON is chosen by ?Format= or the Accept header, and pages
# by ?ContinuationToken=, so those are forwarded and part of the cache key.
# The function says how long to keep each response.
resource "aws_cloudfront_cache_policy" "website_listing" {
  name    = "${var.name}-listing"
  comment = "Directory listings of ${var.website_domain}"

  min_ttl     = 0
  default_ttl = 300
  max_ttl     = 86400

  parameters_in_cache_key_and_forwarded_to_origin {
    enable_accept_encoding_brotli = true
    enable_accept_encoding_gzip   = true

    cookies_config {
      cookie_behavior = "none"
    }
    headers_config {
      header_behavior = "whitelist"
      headers {
        items = ["Accept"]
      }
    }
    query_strings_config {
      query_string_behavior = "whitelist"
      query_strings {
        items = ["ContinuationToken", "Format"]
      }
    }
  }
}

resource "aws_cloudfront_response_headers_policy" "website" {
  name    = var.name
  comment = "Security headers for ${var.website_domain}"

  security_headers_config {
    # Nothing on the site runs scripts. This also defuses any HTML or SVG
    # someone once attached to an issue, which is served from this origin.
    content_security_policy {
      content_security_policy = "default-src 'none'; style-src 'self'; img-src 'self' data:; media-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
      override                = true
    }
    content_type_options {
      override = true
    }
    frame_options {
      frame_option = "DENY"
      override     = true
    }
    referrer_policy {
      referrer_policy = "same-origin"
      override        = true
    }
    strict_transport_security {
      access_control_max_age_sec = 31536000
      include_subdomains         = false
      preload                    = false
      override                   = true
    }
  }

  # For browsers. S3 objects carry no Cache-Control of their own; the listing
  # function's responses do, and keep theirs.
  custom_headers_config {
    items {
      header   = "Cache-Control"
      value    = "public, max-age=${var.website_file_ttl_seconds}"
      override = false
    }
  }
}

resource "aws_cloudfront_function" "website_download" {
  name    = "${var.name}-download"
  comment = "Marks *.json.gz on ${var.website_domain} as downloads"
  runtime = "cloudfront-js-2.0"
  publish = true
  code    = file("${path.module}/download.js")
}

resource "aws_cloudfront_distribution" "website" {
  comment         = var.website_domain
  aliases         = [var.website_domain]
  enabled         = true
  is_ipv6_enabled = true
  http_version    = "http2and3"
  price_class     = var.website_price_class

  # No default root object: "/" is a directory like any other.

  origin {
    origin_id                = local.website_s3_origin
    domain_name              = aws_s3_bucket.backup.bucket_regional_domain_name
    origin_access_control_id = aws_cloudfront_origin_access_control.website_s3.id
  }

  origin {
    origin_id                = local.website_listing_origin
    domain_name              = split("/", aws_lambda_function_url.listing.function_url)[2]
    origin_access_control_id = aws_cloudfront_origin_access_control.website_listing.id

    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  # A path S3 has no object for goes on to the listing function, which knows
  # whether it is a directory missing its trailing slash.
  origin_group {
    origin_id = local.website_files_origin

    failover_criteria {
      status_codes = [403, 404]
    }
    member {
      origin_id = local.website_s3_origin
    }
    member {
      origin_id = local.website_listing_origin
    }
  }

  default_cache_behavior {
    target_origin_id           = local.website_files_origin
    viewer_protocol_policy     = "redirect-to-https"
    allowed_methods            = ["GET", "HEAD", "OPTIONS"]
    cached_methods             = ["GET", "HEAD", "OPTIONS"]
    cache_policy_id            = aws_cloudfront_cache_policy.website_files.id
    response_headers_policy_id = aws_cloudfront_response_headers_policy.website.id
    # Bundles, archives and the .json.gz collections are already compressed.
    compress = false

    function_association {
      event_type   = "viewer-response"
      function_arn = aws_cloudfront_function.website_download.arn
    }
  }

  # First match wins, so the stylesheet comes before the "*/" listings.
  ordered_cache_behavior {
    path_pattern               = "/_listing/*"
    target_origin_id           = local.website_listing_origin
    viewer_protocol_policy     = "redirect-to-https"
    allowed_methods            = ["GET", "HEAD", "OPTIONS"]
    cached_methods             = ["GET", "HEAD", "OPTIONS"]
    cache_policy_id            = aws_cloudfront_cache_policy.website_files.id
    response_headers_policy_id = aws_cloudfront_response_headers_policy.website.id
    compress                   = true
  }

  dynamic "ordered_cache_behavior" {
    for_each = ["/", "*/"]

    content {
      path_pattern               = ordered_cache_behavior.value
      target_origin_id           = local.website_listing_origin
      viewer_protocol_policy     = "redirect-to-https"
      allowed_methods            = ["GET", "HEAD", "OPTIONS"]
      cached_methods             = ["GET", "HEAD", "OPTIONS"]
      cache_policy_id            = aws_cloudfront_cache_policy.website_listing.id
      response_headers_policy_id = aws_cloudfront_response_headers_policy.website.id
      compress                   = true
    }
  }

  viewer_certificate {
    acm_certificate_arn      = aws_acm_certificate_validation.website.certificate_arn
    ssl_support_method       = "sni-only"
    minimum_protocol_version = "TLSv1.2_2021"
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }
}

# --- DNS --------------------------------------------------------------------

resource "aws_route53_record" "website" {
  for_each = toset(["A", "AAAA"])

  zone_id = data.aws_route53_zone.website.zone_id
  name    = var.website_domain
  type    = each.value

  alias {
    name                   = aws_cloudfront_distribution.website.domain_name
    zone_id                = aws_cloudfront_distribution.website.hosted_zone_id
    evaluate_target_health = false
  }
}

locals {
  cloudwatch_alarm_count = var.enable_cloudwatch_alarms ? 1 : 0
  service_alarm_count    = var.enable_cloudwatch_alarms && var.service_desired_count > 0 ? 1 : 0

  alarm_arns = compact([
    try(aws_cloudwatch_metric_alarm.alb_elb_5xx[0].arn, ""),
    try(aws_cloudwatch_metric_alarm.alb_target_5xx[0].arn, ""),
    try(aws_cloudwatch_metric_alarm.alb_unhealthy_hosts[0].arn, ""),
    try(aws_cloudwatch_metric_alarm.alb_target_response_time_p99[0].arn, ""),
    try(aws_cloudwatch_metric_alarm.ecs_cpu_high[0].arn, ""),
    try(aws_cloudwatch_metric_alarm.ecs_memory_high[0].arn, ""),
    try(aws_cloudwatch_metric_alarm.ecs_running_task_shortage[0].arn, ""),
  ])
}

resource "aws_cloudwatch_metric_alarm" "alb_elb_5xx" {
  count = local.cloudwatch_alarm_count

  alarm_name          = "${local.name}-alb-elb-5xx"
  alarm_description   = "ALB generated 5xx responses."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  datapoints_to_alarm = 2
  threshold           = var.alb_5xx_alarm_threshold
  treat_missing_data  = "notBreaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace   = "AWS/ApplicationELB"
  metric_name = "HTTPCode_ELB_5XX_Count"
  statistic   = "Sum"
  period      = 60

  dimensions = {
    LoadBalancer = aws_lb.solver.arn_suffix
  }
}

resource "aws_cloudwatch_metric_alarm" "alb_target_5xx" {
  count = local.cloudwatch_alarm_count

  alarm_name          = "${local.name}-alb-target-5xx"
  alarm_description   = "Solver targets returned 5xx responses."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  datapoints_to_alarm = 2
  threshold           = var.target_5xx_alarm_threshold
  treat_missing_data  = "notBreaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace   = "AWS/ApplicationELB"
  metric_name = "HTTPCode_Target_5XX_Count"
  statistic   = "Sum"
  period      = 60

  dimensions = {
    LoadBalancer = aws_lb.solver.arn_suffix
    TargetGroup  = aws_lb_target_group.solver.arn_suffix
  }
}

resource "aws_cloudwatch_metric_alarm" "alb_unhealthy_hosts" {
  count = local.service_alarm_count

  alarm_name          = "${local.name}-unhealthy-hosts"
  alarm_description   = "At least one ALB target is unhealthy."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 3
  datapoints_to_alarm = 2
  threshold           = 0
  treat_missing_data  = "breaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace   = "AWS/ApplicationELB"
  metric_name = "UnHealthyHostCount"
  statistic   = "Maximum"
  period      = 60

  dimensions = {
    LoadBalancer = aws_lb.solver.arn_suffix
    TargetGroup  = aws_lb_target_group.solver.arn_suffix
  }
}

resource "aws_cloudwatch_metric_alarm" "alb_target_response_time_p99" {
  count = local.cloudwatch_alarm_count

  alarm_name          = "${local.name}-target-p99-latency"
  alarm_description   = "Solver target response time p99 is above threshold."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  datapoints_to_alarm = 3
  threshold           = var.target_response_time_p99_alarm_threshold_seconds
  treat_missing_data  = "notBreaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace          = "AWS/ApplicationELB"
  metric_name        = "TargetResponseTime"
  extended_statistic = "p99"
  period             = 60

  dimensions = {
    LoadBalancer = aws_lb.solver.arn_suffix
    TargetGroup  = aws_lb_target_group.solver.arn_suffix
  }
}

resource "aws_cloudwatch_metric_alarm" "ecs_cpu_high" {
  count = local.service_alarm_count

  alarm_name          = "${local.name}-ecs-cpu-high"
  alarm_description   = "Average ECS service CPU utilization is high."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  datapoints_to_alarm = 3
  threshold           = var.ecs_cpu_alarm_threshold_percent
  treat_missing_data  = "notBreaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace   = "AWS/ECS"
  metric_name = "CPUUtilization"
  statistic   = "Average"
  period      = 60

  dimensions = {
    ClusterName = aws_ecs_cluster.solver.name
    ServiceName = aws_ecs_service.solver.name
  }
}

resource "aws_cloudwatch_metric_alarm" "ecs_memory_high" {
  count = local.service_alarm_count

  alarm_name          = "${local.name}-ecs-memory-high"
  alarm_description   = "Average ECS service memory utilization is high."
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 5
  datapoints_to_alarm = 3
  threshold           = var.ecs_memory_alarm_threshold_percent
  treat_missing_data  = "notBreaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace   = "AWS/ECS"
  metric_name = "MemoryUtilization"
  statistic   = "Average"
  period      = 60

  dimensions = {
    ClusterName = aws_ecs_cluster.solver.name
    ServiceName = aws_ecs_service.solver.name
  }
}

resource "aws_cloudwatch_metric_alarm" "ecs_running_task_shortage" {
  count = local.service_alarm_count

  alarm_name          = "${local.name}-ecs-running-task-shortage"
  alarm_description   = "ECS running task count is below desired count."
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 3
  datapoints_to_alarm = 2
  threshold           = var.service_desired_count
  treat_missing_data  = "notBreaching"
  alarm_actions       = var.alarm_actions
  ok_actions          = var.ok_actions

  namespace   = "ECS/ContainerInsights"
  metric_name = "RunningTaskCount"
  statistic   = "Minimum"
  period      = 60

  dimensions = {
    ClusterName = aws_ecs_cluster.solver.name
    ServiceName = aws_ecs_service.solver.name
  }
}

resource "aws_cloudwatch_dashboard" "solver" {
  count = var.enable_cloudwatch_dashboard ? 1 : 0

  dashboard_name = local.name
  dashboard_body = jsonencode({
    start          = "-PT8H"
    periodOverride = "inherit"
    widgets = [
      {
        type   = "text"
        x      = 0
        y      = 0
        width  = 24
        height = 2
        properties = {
          markdown = "# ${local.name}\n${try(aws_lb.solver.dns_name, "")}\nImage: `${local.container_image}`"
        }
      },
      {
        type   = "alarm"
        x      = 0
        y      = 2
        width  = 24
        height = 4
        properties = {
          title  = "Alarm Status"
          alarms = local.alarm_arns
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 6
        width  = 12
        height = 6
        properties = {
          title   = "ALB Requests and 5xx"
          region  = var.aws_region
          view    = "timeSeries"
          stacked = false
          metrics = [
            ["AWS/ApplicationELB", "RequestCount", "LoadBalancer", aws_lb.solver.arn_suffix, { stat = "Sum", label = "requests" }],
            [".", "HTTPCode_ELB_5XX_Count", ".", ".", { stat = "Sum", label = "alb 5xx" }],
            [".", "HTTPCode_Target_5XX_Count", ".", ".", "TargetGroup", aws_lb_target_group.solver.arn_suffix, { stat = "Sum", label = "target 5xx" }],
          ]
          period = 60
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 6
        width  = 12
        height = 6
        properties = {
          title   = "ALB Target Health and Latency"
          region  = var.aws_region
          view    = "timeSeries"
          stacked = false
          metrics = [
            ["AWS/ApplicationELB", "HealthyHostCount", "TargetGroup", aws_lb_target_group.solver.arn_suffix, "LoadBalancer", aws_lb.solver.arn_suffix, { stat = "Minimum", label = "healthy targets" }],
            [".", "UnHealthyHostCount", ".", ".", ".", ".", { stat = "Maximum", label = "unhealthy targets" }],
            [".", "TargetResponseTime", ".", ".", ".", ".", { stat = "p99", label = "p99 latency" }],
          ]
          period = 60
        }
      },
      {
        type   = "metric"
        x      = 0
        y      = 12
        width  = 12
        height = 6
        properties = {
          title   = "ECS CPU and Memory"
          region  = var.aws_region
          view    = "timeSeries"
          stacked = false
          metrics = [
            ["AWS/ECS", "CPUUtilization", "ClusterName", aws_ecs_cluster.solver.name, "ServiceName", aws_ecs_service.solver.name, { stat = "Average", label = "cpu %" }],
            [".", "MemoryUtilization", ".", ".", ".", ".", { stat = "Average", label = "memory %" }],
          ]
          period = 60
        }
      },
      {
        type   = "metric"
        x      = 12
        y      = 12
        width  = 12
        height = 6
        properties = {
          title   = "ECS Tasks"
          region  = var.aws_region
          view    = "timeSeries"
          stacked = false
          metrics = [
            ["ECS/ContainerInsights", "RunningTaskCount", "ClusterName", aws_ecs_cluster.solver.name, "ServiceName", aws_ecs_service.solver.name, { stat = "Minimum", label = "running tasks" }],
            [".", "DesiredTaskCount", ".", ".", ".", ".", { stat = "Maximum", label = "desired tasks" }],
          ]
          period = 60
        }
      },
      {
        type   = "log"
        x      = 0
        y      = 18
        width  = 24
        height = 6
        properties = {
          title  = "Recent Warnings and Errors"
          region = var.aws_region
          view   = "table"
          query  = "SOURCE '${aws_cloudwatch_log_group.solver.name}' | fields @timestamp, @logStream, @message | filter @message like /(?i)(error|warn|failed|rejected|panic)/ | sort @timestamp desc | limit 50"
        }
      },
    ]
  })
}

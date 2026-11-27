import type { MetricLayout, MetricPolicy } from './types';

/** Whether the Card can report data for this metric. Without a policy, every metric shows. */
export function policyShows(policy: MetricPolicy | undefined, metricId: string) {
  return !policy || policy.visibleMetricIds.includes(metricId);
}

/**
 * Whether the Star control applies to this metric: new Stars go only on starrable metrics, and a
 * Star already given on a visible metric can be removed. Settings only keep Stars on pinnable
 * metrics, so a given Star needs no pinnable check.
 */
export function canToggleStar(policy: MetricPolicy | undefined, metric: MetricLayout) {
  if (!policy) return false;
  return (
    policy.starrableMetricIds.includes(metric.id) ||
    (metric.pinned && policy.visibleMetricIds.includes(metric.id))
  );
}

/** Whether another Star fits under the limit. Only Stars on visible metrics count toward it. */
export function canAddStar(policy: MetricPolicy | undefined, metrics: MetricLayout[]) {
  if (!policy) return false;
  const stars = metrics.filter(
    (metric) => metric.pinned && policy.visibleMetricIds.includes(metric.id),
  ).length;
  return stars < policy.maxStars;
}

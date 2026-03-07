import time
import math

def smooth_damp(current, target, current_velocity, smooth_time, delta_time, max_speed=float('inf')):
    smooth_time = max(0.0001, smooth_time)
    omega = 2.0 / smooth_time
    x = omega * delta_time
    exp = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x)
    change = current - target
    original_target = target

    max_change = max_speed * smooth_time
    change = max(-max_change, min(max_change, change))

    target = current - change
    temp = (current_velocity + omega * change) * delta_time
    current_velocity = (current_velocity - omega * temp) * exp
    output = target + (change + temp) * exp

    if (original_target - current > 0.0) == (output > original_target):
        output = original_target
        current_velocity = (output - original_target) / delta_time

    return output, current_velocity

class MistTransform:
    def __init__(self, is_owner=False):
        self.is_owner = is_owner

        self.base_sync_interval = 0.1
        self.near_distance = 100.0
        self.far_distance = 400.0
        self.far_sync_interval = 0.5

        self.smooth_time = 0.12
        self.extrapolation_decay = 0.8

        self.position = [0.0, 0.0]
        self.velocity = [0.0, 0.0]
        self.target_position = [0.0, 0.0]
        self.received_velocity = [0.0, 0.0]
        self.current_velocity = [0.0, 0.0]

        self.current_sync_interval = self.base_sync_interval
        self.time_since_last_receive = 0.0
        self.last_received_sequence = -1

    def is_newer_sequence(self, incoming, last):
        if last == -1: return True
        diff = (incoming - last) & 0xFFFF
        return diff < 0x8000 and diff > 0

    def calculate_sync_interval(self, self_pos):
        if self.is_owner: return self.base_sync_interval

        dist = math.sqrt((self.position[0] - self_pos[0])**2 + (self.position[1] - self_pos[1])**2)
        if dist <= self.near_distance:
            return self.base_sync_interval
        if dist >= self.far_distance:
            return self.far_sync_interval

        t = (dist - self.near_distance) / (far_dist := self.far_distance - self.near_distance)
        return self.base_sync_interval + (self.far_sync_interval - self.base_sync_interval) * t

    def receive_location(self, data, current_time):
        seq = data.get("sequence", 0)
        if not self.is_newer_sequence(seq, self.last_received_sequence):
            return

        self.last_received_sequence = seq
        pos = data["position"]
        self.target_position = [pos["x"], pos["y"]]
        vel = data.get("velocity", {"x": 0, "y": 0})
        self.received_velocity = [vel["x"], vel["y"]]
        self.current_sync_interval = data.get("time", self.base_sync_interval)
        self.time_since_last_receive = 0.0

    def update_interpolation(self, dt):
        if self.is_owner: return

        self.time_since_last_receive += dt

        extrapolation_factor = max(0.0, min(1.0, 1.0 - (self.time_since_last_receive / self.current_sync_interval)))
        extrapolation_factor *= self.extrapolation_decay

        self.target_position[0] += self.received_velocity[0] * dt * extrapolation_factor
        self.target_position[1] += self.received_velocity[1] * dt * extrapolation_factor

        self.position[0], self.current_velocity[0] = smooth_damp(
            self.position[0], self.target_position[0], self.current_velocity[0], self.smooth_time, dt
        )
        self.position[1], self.current_velocity[1] = smooth_damp(
            self.position[1], self.target_position[1], self.current_velocity[1], self.smooth_time, dt
        )

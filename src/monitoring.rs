use crate::{api, config};

pub enum SingleMachineMonitoringState {
    /// The machine's timer has been recently reset.
    /// Contained is the Unixtime the machine has set.
    Ok(std::time::SystemTime),

    /// The machine's timer value hasn't been received yet.
    NoData,

    /// The machine's timer is unusually far into the future.
    TooFar(std::time::SystemTime),

    /// The machine's timer has elapsed, and we are now in the grace period.
    /// Final reset will happen at the given Unixtime.
    GracePeriod(std::time::SystemTime),

    /// We have reset the machine, and are waiting for it to come back online.
    /// Resuming monitoring after the given Unixtime.
    Resetting(std::time::SystemTime),

    /// The machine is powered off, so monitoring should not happen.
    PowerOff,
}

const THRESHOLDS: &[(u64, &str)] = &[
    (60, "1 minute"),
    (120, "2 minutes"),
    (180, "3 minutes"),
    (240, "4 minutes"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (900, "15 minutes"),
    (1800, "30 minutes"),
    (3600, "1 hour"),
    (7200, "2 hours"),
];

pub struct SingleMachineMonitoring {
    state: SingleMachineMonitoringState,
    config: config::VmConfig,

    api: api::Api,

    tg_client: reqwest::Client,

    /// How many times in a row has the guest agent ping failed?
    ping_fail_count: u32,

    /// The shortest threshold that we've sent a message about grace period for.
    /// None if we haven't sent a message yet.
    last_sent_threshold: Option<u64>,
}

impl SingleMachineMonitoring {
    pub fn new(api: api::Api, config: config::VmConfig) -> Self {
        Self {
            state: SingleMachineMonitoringState::NoData,
            config,
            api,
            ping_fail_count: 0,
            last_sent_threshold: None,
            tg_client: reqwest::Client::new(),
        }
    }

    pub async fn tick(&mut self) {
        let is_machine_running = self.api.get_is_machine_running(&self.config).await;

        match (is_machine_running, &self.state) {
            (Ok(false), SingleMachineMonitoringState::PowerOff) => {
                tracing::debug!("Machine is still powered off, nothing to do.");
                return;
            }
            (Ok(true), SingleMachineMonitoringState::PowerOff) => {
                // Machine is now powered on, start monitoring.
                tracing::debug!("Machine was off, is now on");
                self.say("Machine has been powered on, beginnning reset timer")
                    .await;
                self.state = SingleMachineMonitoringState::Resetting(
                    std::time::SystemTime::now()
                        + std::time::Duration::from_secs(self.config.reset_duration),
                );
                self.ping_fail_count = 0;
            }
            (Ok(true), _) => {
                // Machine is still powered on
            }
            (Ok(false), _) => {
                tracing::debug!("Machine is now powered off, and we are still monitoring");
                // Machine is now powered off, stop monitoring.
                self.say("Machine has been powered off, stopping monitoring")
                    .await;
                self.state = SingleMachineMonitoringState::PowerOff;
                return;
            }
            (Err(why), _) => {
                // Error getting is_machine_running, just continue
                tracing::error!("Failed to get is_machine_running: {}", why);
                return;
            }
        }

        // If we are not in GracePeriod,
        // reset last_sent_threshold.
        if !matches!(self.state, SingleMachineMonitoringState::GracePeriod(_)) {
            self.last_sent_threshold = None;
        }

        // If the machine is currently resetting, just wait until it's reset.
        if let SingleMachineMonitoringState::Resetting(reset_time) = self.state {
            if std::time::SystemTime::now() >= reset_time {
                // The machine has reset,
                // so resume monitoring.
                self.say("Machine reset timer has completed, resuming monitoring")
                    .await;
                self.state = SingleMachineMonitoringState::NoData;
            }
        }

        // If the machine was too far, but that state has now passed,
        // then it's back to normal.
        if let SingleMachineMonitoringState::TooFar(reset_time) = self.state {
            if std::time::SystemTime::now()
                + std::time::Duration::from_secs(self.config.max_no_warning_interval)
                >= reset_time
            {
                self.state = SingleMachineMonitoringState::Ok(reset_time);
            }
        }

        // Always ping the machine first.
        match self.api.ping_guest_agent(&self.config).await {
            Ok(()) => {
                self.ping_fail_count = 0;
            }
            Err(e) => {
                tracing::info!("VMID {} ping failed: {}", self.config.vmid, e);
                self.ping_fail_count += 1;

                // If the machine failed 5 pings in a row,
                // then move it to the grace period
                // if it's not there already.
                if let SingleMachineMonitoringState::Ok(_) = self.state {
                    if self.ping_fail_count >= 5 {
                        self.state = SingleMachineMonitoringState::GracePeriod(
                            std::time::SystemTime::now()
                                + std::time::Duration::from_secs(self.config.grace_period),
                        );

                        self.say("The machine has failed to respond to 5 QEMU guest-agent pings in a row. Grace period started").await;
                    }
                }
            }
        }

        if self.ping_fail_count == 0 {
            // Ping was successful,
            // now write the current time into the guest.
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string();

            if let Err(why) = self
                .api
                .guest_agent_write_file(
                    &self.config,
                    "/tmp/watchdog_current_unix_time",
                    &current_time.as_bytes(),
                )
                .await
            {
                tracing::info!(
                    "VMID {} write_file /tmp/watchdog_current_unix_time failed: {}",
                    self.config.vmid,
                    why
                );

                // Failing writes are a big deal, so move it to the grace period right away.
                if let SingleMachineMonitoringState::Ok(_) = self.state {
                    self.state = SingleMachineMonitoringState::GracePeriod(
                        std::time::SystemTime::now()
                            + std::time::Duration::from_secs(self.config.grace_period),
                    );
                    self.say("Watchdog failed to write the current time to the guest into /tmp/watchdog_current_unix_time. Grace period started").await;
                }
            } else {
                // Write was successful,
                // now read the reset time from the guest.
                match self
                    .api
                    .guest_agent_read_file(&self.config, "/tmp/watchdog_reset_after")
                    .await
                {
                    Err(why) => {
                        println!(
                            "VMID {} read_file /tmp/watchdog_reset_after failed: {}",
                            self.config.vmid, why
                        );

                        // Failed reads move to the grace period immediately.
                        if let SingleMachineMonitoringState::Ok(_) = self.state {
                            self.state = SingleMachineMonitoringState::GracePeriod(
                                std::time::SystemTime::now()
                                    + std::time::Duration::from_secs(self.config.grace_period),
                            );

                            self.say("Watchdog failed to read the reset time from the guest into /tmp/watchdog_reset_after. Perhaps the file doesn't exist? Grace period started").await;
                        }
                    }
                    Ok(reset_time) => {
                        match reset_time.trim().parse::<u64>() {
                            Err(why) => {
                                tracing::info!(
                                    "VMID {} failed to parse reset time: {}",
                                    self.config.vmid,
                                    why
                                );

                                // Failed parses move to the grace period immediately.
                                if let SingleMachineMonitoringState::Ok(_) = self.state {
                                    self.state = SingleMachineMonitoringState::GracePeriod(
                                        std::time::SystemTime::now()
                                            + std::time::Duration::from_secs(
                                                self.config.grace_period,
                                            ),
                                    );
                                    self.say("Watchdog failed to parse /tmp/watchdog_reset_after as a Unix time. Grace period started").await;
                                    self.say(&format!(
                                        "The current text in /tmp/watchdog_reset_after is: \n\n```\n{}\n```",
                                        &reset_time,
                                    ))
                                    .await;
                                }
                            }

                            Ok(reset_time) => {
                                // The machine has successfully given us a reset time.
                                let reset_time = std::time::SystemTime::UNIX_EPOCH
                                    + std::time::Duration::from_secs(reset_time);

                                // How many seconds until the reset time?
                                let seconds_until_reset = reset_time
                                    .duration_since(std::time::SystemTime::now())
                                    .unwrap_or_default()
                                    .as_secs();

                                // If too many, then it's in the TooFar state.
                                if seconds_until_reset > self.config.max_no_warning_interval {
                                    if !matches!(
                                        self.state,
                                        SingleMachineMonitoringState::TooFar(_)
                                    ) {
                                        let reset_time: chrono::DateTime<chrono::Utc> =
                                            chrono::DateTime::from(reset_time);
                                        self.say(format!("Machine requested reset at {}, which is too far into the future. This is OK if you are performing manual maintenance.", reset_time).as_str()).await;
                                    }
                                    self.state = SingleMachineMonitoringState::TooFar(reset_time);
                                }
                                // Otherwise, if the time is in the future, then it's in the Ok state.
                                else if seconds_until_reset > 0 {
                                    if !matches!(self.state, SingleMachineMonitoringState::Ok(_)) {
                                        self.say("Machine is OK").await;
                                    }
                                    self.state = SingleMachineMonitoringState::Ok(reset_time);
                                }
                            }
                        }
                    }
                }
            }
        }

        // If the Ok time is in the past,
        // then move it to the grace period.
        if let SingleMachineMonitoringState::Ok(reset_time) = self.state {
            if reset_time <= std::time::SystemTime::now() {
                self.state = SingleMachineMonitoringState::GracePeriod(
                    std::time::SystemTime::now()
                        + std::time::Duration::from_secs(self.config.grace_period),
                );

                let reset_time: chrono::DateTime<chrono::Utc> = chrono::DateTime::from(reset_time);
                self.say(&format!("Machine has not updated its /tmp/watchdog_reset_after in a while (last update was at {reset_time}). Grace period started"))
                    .await;
            }
        }

        // If the state is NoData,
        // then that means that we haven't yet been able to read a value,
        // so we start the grace period immediately.
        if let SingleMachineMonitoringState::NoData = self.state {
            self.state = SingleMachineMonitoringState::GracePeriod(
                std::time::SystemTime::now()
                    + std::time::Duration::from_secs(self.config.grace_period),
            );
            self.say("Could not read the next reset time from the file at /tmp/watchdog_reset_after. Grace period started")
                .await;
        }

        // If the state is GracePeriod,
        // and the reset time is in the past,
        // then move it to the Resetting state.
        if let SingleMachineMonitoringState::GracePeriod(reset_time) = self.state {
            if reset_time <= std::time::SystemTime::now() {
                self.state = SingleMachineMonitoringState::Resetting(
                    std::time::SystemTime::now()
                        + std::time::Duration::from_secs(self.config.reset_duration),
                );
                self.say("Grace period has expired. Resetting machine now")
                    .await;

                if self.config.dry_run {
                    self.say("Dry-run mode: not actually resetting the machine")
                        .await;
                } else {
                    if let Err(why) = self.api.reset_vm(&self.config).await {
                        self.say(&format!("Failed to reset machine: {}", why.to_string()))
                            .await;
                    }
                }
            }
        }

        // If the state is GracePeriod,
        // then check thresholds.
        if let SingleMachineMonitoringState::GracePeriod(reset_time) = self.state {
            let seconds_until_reset = reset_time
                .duration_since(std::time::SystemTime::now())
                .unwrap_or_default()
                .as_secs();

            let mut closest_without_going_under = THRESHOLDS.last().unwrap();

            for threshold in THRESHOLDS {
                if threshold.0 > seconds_until_reset {
                    closest_without_going_under = threshold;
                    break;
                }
            }

            if let None = self.last_sent_threshold {
                self.last_sent_threshold = Some(closest_without_going_under.0);
                self.say(&format!(
                    "Machine will reset in {} unless the issue is fixed",
                    closest_without_going_under.1
                ))
                .await;
            } else if let Some(last_sent_threshold) = self.last_sent_threshold {
                if last_sent_threshold != closest_without_going_under.0 {
                    self.last_sent_threshold = Some(closest_without_going_under.0);
                    self.say(&format!(
                        "Machine will reset in {} unless the issue is fixed",
                        closest_without_going_under.1
                    ))
                    .await;
                }
            }
        }
    }

    pub async fn say(&self, message: &str) {
        tracing::info!("MSG: {}", message);

        if let (Some(token), Some(chat_id)) = (
            &self.config.telegram_bot_token,
            &self.config.telegram_chat_id,
        ) {
            let message = format!(
                "VMID {} ({}): {}",
                self.config.vmid, self.config.friendly_name, message
            );

            let url = format!("https://api.telegram.org/bot{token}/sendMessage");

            let res = self
                .tg_client
                .post(url)
                .json(&serde_json::json!({"chat_id": chat_id, "text": message}))
                .send()
                .await;
            if let Err(why) = res {
                println!("Failed to send message: {}", why);
            }
        }
    }
}

package com.example.demo;

import org.springframework.boot.ApplicationArguments;
import org.springframework.boot.ApplicationRunner;
import org.springframework.stereotype.Component;

@Component
public class SlowStartupRunner implements ApplicationRunner {

    @Override
    public void run(ApplicationArguments args) throws InterruptedException {
        // Simulate slow initialization (e.g. warming caches, connecting to dependencies)
        // so this app is a realistic blue-green deployment test candidate.
        Thread.sleep(10_000);
    }
}

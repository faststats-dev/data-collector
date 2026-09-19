interface Job {
  label: string;
  attempts: number;
}

class Runner {
  run(job: Job): never {
    return this.fail(`${job.label}: ${job.attempts}`);
  }

  private fail(message: string): never {
    throw new Error(message);
  }
}

function dispatch(job: Job): never {
  return new Runner().run(job);
}

dispatch({label: 'typed fixture', attempts: 3});

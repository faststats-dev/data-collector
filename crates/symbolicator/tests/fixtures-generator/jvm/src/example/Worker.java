package example;

class Worker {
    void run(int value) {
        Helper.fail("fixture crash " + value);
    }
}

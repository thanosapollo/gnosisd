;;; export-review-parity.el --- Export live Gnosis review vectors  -*- lexical-binding: t; -*-

(require 'json)
(require 'gnosis)
(require 'gnosis-algorithm)
(require 'gnosis-custom-values)
(require 'gnosis-history-schema)

(defun gnosis-core-parity--parameters ()
  (gnosis-algorithm-parameters-create
   :scheduler-version 1 :amnesia 0.5 :epignosis 0.1 :agnoia 0.2
   :anagnosis 3 :lethe 2 :synolon-max 3.0 :interval-fuzz 0.1
   :proto '(0 1 2)))

(defun gnosis-core-parity--new-state ()
  (gnosis-algorithm-state-create
   :state-revision 0 :review-count 0 :legacy-review-count 0
   :legacy-unclassified-count 0 :last-reviewed-at-us nil
   :last-review-day nil :next-review-day 20260715 :interval-days nil
   :consecutive-successes 0 :total-successes 0
   :consecutive-failures 0 :total-failures 0 :suspended 0
   :gnosis-plus 0.35 :gnosis-minus 0.30 :gnosis-synolon 1.3))

(defun gnosis-core-parity--transition (name state success fuzz-delta)
  (let* ((transition
          (gnosis-algorithm-compute-transition
           :state state :parameters (gnosis-core-parity--parameters)
           :success success :review-day 20260715
           :reviewed-at-us 1752600000000000 :fuzz-delta fuzz-delta))
         (after (gnosis-algorithm-transition-after transition)))
    `((name . ,name)
      (algorithm_interval_days
       . ,(gnosis-algorithm-transition-algorithm-interval-days transition))
      (computed_interval_days
       . ,(gnosis-algorithm-transition-computed-interval-days transition))
      (fuzz_delta . ,(gnosis-algorithm-transition-fuzz-delta transition))
      (next_review_day . ,(gnosis-algorithm-state-next-review-day after))
      (gnosis_plus . ,(gnosis-algorithm-state-gnosis-plus after))
      (gnosis_minus . ,(gnosis-algorithm-state-gnosis-minus after))
      (gnosis_synolon . ,(gnosis-algorithm-state-gnosis-synolon after))
      (total_successes . ,(gnosis-algorithm-state-total-successes after))
      (total_failures . ,(gnosis-algorithm-state-total-failures after)))))

(defun gnosis-core-parity--mature-state ()
  (let ((state (gnosis-core-parity--new-state)))
    (setf (gnosis-algorithm-state-state-revision state) 8
          (gnosis-algorithm-state-review-count state) 8
          (gnosis-algorithm-state-last-reviewed-at-us state) 1
          (gnosis-algorithm-state-last-review-day state) 20260705
          (gnosis-algorithm-state-interval-days state) 10
          (gnosis-algorithm-state-total-successes state) 8
          (gnosis-algorithm-state-consecutive-successes state) 8
          (gnosis-algorithm-state-gnosis-synolon state) 2.3)
    state))

(defun gnosis-core-parity--reviewed-state ()
  (let ((state (gnosis-core-parity--new-state)))
    (setf (gnosis-algorithm-state-state-revision state) 1
          (gnosis-algorithm-state-review-count state) 1
          (gnosis-algorithm-state-last-reviewed-at-us state) 1
          (gnosis-algorithm-state-last-review-day state) 20260714
          (gnosis-algorithm-state-interval-days state) 0
          (gnosis-algorithm-state-total-successes state) 1
          (gnosis-algorithm-state-consecutive-successes state) 1)
    state))

(defun gnosis-core-parity--lethe-state ()
  (let ((state (gnosis-core-parity--mature-state)))
    (setf (gnosis-algorithm-state-total-successes state) 6
          (gnosis-algorithm-state-consecutive-successes state) 0
          (gnosis-algorithm-state-total-failures state) 2
          (gnosis-algorithm-state-consecutive-failures state) 2)
    state))

(defun gnosis-core-parity--due-floor-state ()
  (let ((state (gnosis-core-parity--mature-state)))
    (setf (gnosis-algorithm-state-next-review-day state) 20260815)
    state))

(defconst gnosis-core-parity--tag-values
  '((:tag "Amboss" (:proto (1 3) :anagnosis 3 :amnesia 0.45
                            :epignosis 0.5 :agnoia 0.4 :lethe 2))
    (:tag "Bulgarian" (:amnesia 0.55 :proto (0 1 3) :lethe 2))
    (:tag "Unking" (:proto (1 3) :amnesia 0.45))
    (:tag "vocabulary" (:amnesia 0.65 :proto (0 1 3)))))

(defun gnosis-core-parity--tag-rules ()
  (let ((tags '("Amboss" "Unking")))
    `((tags . ,(vconcat tags))
      (amnesia . ,(gnosis-get-thema-amnesia
                   nil tags gnosis-core-parity--tag-values))
      (epignosis . ,(gnosis-get-thema-epignosis
                     nil tags gnosis-core-parity--tag-values))
      (agnoia . ,(gnosis-get-thema-agnoia
                  nil tags gnosis-core-parity--tag-values))
      (anagnosis . ,(gnosis-get-thema-anagnosis
                     nil tags gnosis-core-parity--tag-values))
      (lethe . ,(gnosis-get-thema-lethe
                 nil tags gnosis-core-parity--tag-values))
      (proto . ,(vconcat
                 (gnosis-get-thema-proto
                  nil tags gnosis-core-parity--tag-values))))))

(defun gnosis-core-parity--logical-days ()
  (vector
   `((name . "before_boundary")
     (review_day . ,(gnosis-history--logical-day 1772319000000000 7200 3)))
   `((name . "dst_before")
     (review_day . ,(gnosis-history--logical-day 1774744200000000 7200 0)))
   `((name . "dst_after")
     (review_day . ,(gnosis-history--logical-day 1774747800000000 10800 0)))))

(let ((gnosis-string-difference 1))
  (princ
   (json-serialize
    `((matcher
       . ,(vector
           `((name . "case_whitespace")
             (matches . ,(if (gnosis-compare-strings " Answer " "answer") t :false)))
           `((name . "quotes")
             (matches . ,(if (gnosis-compare-strings "\"answer\"" "answer") t :false)))
           `((name . "distance_one")
             (matches . ,(if (gnosis-compare-strings "answer" "answe") t :false)))
           `((name . "short_exact")
             (matches . ,(if (gnosis-compare-strings "a" "b") t :false)))))
      (scheduler
       . ,(vector
           (gnosis-core-parity--transition
            "first_success" (gnosis-core-parity--new-state) t 0.0)
           (gnosis-core-parity--transition
            "first_failure" (gnosis-core-parity--new-state) nil 0.0)
           (gnosis-core-parity--transition
            "proto_success" (gnosis-core-parity--reviewed-state) t 0.0)
           (gnosis-core-parity--transition
            "proto_failure" (gnosis-core-parity--reviewed-state) nil 0.0)
           (gnosis-core-parity--transition
            "mature_success_fuzz_max" (gnosis-core-parity--mature-state) t 0.1)
           (gnosis-core-parity--transition
            "mature_failure" (gnosis-core-parity--mature-state) nil -0.05)
           (gnosis-core-parity--transition
            "lethe_failure" (gnosis-core-parity--lethe-state) nil 0.0)
           (gnosis-core-parity--transition
            "due_floor" (gnosis-core-parity--due-floor-state) t 0.0)))
      (tag_rules . ,(gnosis-core-parity--tag-rules))
      (logical_days . ,(gnosis-core-parity--logical-days))))))

;;; export-review-parity.el ends here
